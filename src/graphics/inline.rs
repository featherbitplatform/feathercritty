// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The OSC 1337 inline-image parser (feathercritty).
//!
//! [`InlineImageParser`] is fed by the inline-image callbacks of [`crate::vte`]:
//! `inline_image_start(args)` calls [`start`](InlineImageParser::start) with the bytes between
//! `File=` and the first `:` of the string, every `inline_image_put` a
//! [`put`](InlineImageParser::put), `inline_image_end` an [`end`](InlineImageParser::end) (which
//! may return an [`InlineImage`]) and `inline_image_abort` an [`abort`](InlineImageParser::abort).
//! An abort can arrive with no start — the vendored parser passes its argument limit while the
//! arguments are still being read — and a lone abort is a dropped image, not an unfinished one.
//!
//! A sequence is `ESC ] 1337 ; File = <args> : <base64 payload> BEL|ST`. The arguments are
//! `key=value` pairs separated by `;`: the parser keeps separators inside its argument text, so
//! they are split here. Values are plain text except `name`, which is base64. What this terminal
//! acts on is `size` (the payload's byte count, an advisory that refuses up front what the payload
//! cap could not hold anyway), `width` and `height` (a cell count `N`, `Npx`, `N%` or `auto`, kept
//! as [`Dim`] for the embedder to turn into a span), `preserveAspectRatio` (`0` stretches, `1` —
//! the default — keeps the ratio) and `inline`. `name` is read and dropped: nothing here shows a
//! filename. Keys this terminal does not act on (`format`, `inband`, …) are skipped, and a value
//! that is not one of the defined forms leaves its key at its default: the protocol asks for
//! unknown arguments to be ignored, there is no reply channel to complain through, and a default
//! beats refusing an image over a detail. A key sent twice keeps its last value.
//!
//! Display-only, like the rest of this fork: `inline=0` downloads the file without showing it, and
//! a sequence that never says otherwise asks for the same thing. Both are discarded, and their
//! payload streams past without being stored.
//!
//! The payload is base64 text (the vendored parser drops the control characters inside an OSC
//! string, so line breaks never arrive here). It is decoded as it arrives: at most three characters
//! — an incomplete quad — are carried between puts, and a final group decodes with or without its
//! padding characters. Padding that does not end the payload is not the base64 the protocol sends,
//! so such a payload drops the image instead of being guessed at.
//!
//! Format and pixel size come from the header (the `imagesize` crate): PNG, JPEG and GIF, an
//! animated file contributing its first frame; anything else is no image. A file cut short behind a
//! readable header is still handed on — whether the encoded data was complete is the embedder's
//! decode to report, not something the header can say. An image is refused when a side passes
//! [`MAX_IMAGE_SIDE`] or one RGBA8 frame of it would pass [`MAX_IMAGE_BYTES`] (what the embedder
//! decodes into), and a payload past [`MAX_PAYLOAD_BYTES`] is dropped. Every drop releases what it
//! held at once, so no input can grow this parser past one payload cap, and nothing here panics or
//! hangs on remote bytes. A sequence that carries no payload displays nothing.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};

use super::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE, MAX_PAYLOAD_BYTES};

/// A requested display size, in the unit the sender chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dim {
    /// `auto`, and the default when the argument is absent: the image's own pixels.
    #[default]
    Auto,
    /// `N`: cells.
    Cells(u32),
    /// `Npx`: pixels.
    Pixels(u32),
    /// `N%`: percent of the terminal (of its width for `width`, its height for `height`).
    Percent(u32),
}

/// A completed inline image: the payload bytes plus what the arguments asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    /// The encoded data: a PNG, JPEG or GIF file (an animated one contributes its first frame).
    pub bytes: Vec<u8>,
    /// The pixel size read from the header.
    pub pixel_size: (u32, u32),
    /// The `width` argument (`Dim::Auto` when it was absent or unreadable).
    pub width: Dim,
    /// The `height` argument.
    pub height: Dim,
    /// `preserveAspectRatio`: keep the image's ratio while fitting it to `width`/`height`.
    pub preserve_aspect: bool,
    /// Always true of a returned image: a download (`inline=0`, or no `inline` at all) is
    /// discarded here rather than reported with the flag cleared.
    pub inline: bool,
}

/// What the arguments of a sequence asked for. `inline=1` is what makes a sequence worth keeping at
/// all (a download stores nothing), so the flag itself is not carried here; it reaches
/// [`InlineImage::inline`] as a constant. Use [`Args::new`] for the protocol's defaults, which a
/// derived `Default` would get wrong for `preserve_aspect`.
#[derive(Clone, Copy)]
struct Args {
    width: Dim,
    height: Dim,
    preserve_aspect: bool,
}

impl Args {
    /// The protocol's defaults: the image's own pixels, its ratio kept.
    fn new() -> Args {
        Args { width: Dim::Auto, height: Dim::Auto, preserve_aspect: true }
    }
}

/// The sequence in progress: what its arguments asked for, and the payload decoded so far.
struct Sequence {
    args: Args,
    /// Decoded bytes, held within [`MAX_PAYLOAD_BYTES`] by `put`.
    out: Vec<u8>,
}

/// The OSC 1337 inline-image state machine; see the [module docs](self).
#[derive(Default)]
pub struct InlineImageParser {
    /// The sequence being read: `None` between its `end`/`abort` and the next `start`, for one
    /// that was refused or dropped along the way, and for a stray callback with nothing in
    /// progress.
    seq: Option<Sequence>,
    /// At most three base64 characters left over between puts (an incomplete quad).
    stash: Vec<u8>,
}

impl InlineImageParser {
    /// A parser with no sequence in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// An inline image began (`inline_image_start`): its arguments, and the payload after them.
    ///
    /// A sequence left unterminated (a `start` whose `end`/`abort` never came) is discarded first,
    /// and arguments that ask for nothing to display leave no sequence at all, so the payload is
    /// consumed without being stored.
    pub fn start(&mut self, args: &[u8]) {
        self.stash.clear();
        self.seq = parse_args(args).map(|args| Sequence { args, out: Vec::new() });
    }

    /// Payload text arrived (`inline_image_put`), in arbitrary slices.
    pub fn put(&mut self, bytes: &[u8]) {
        let Some(mut seq) = self.seq.take() else { return }; // consumed, or never started
        let decoded = match decode_prefix(&mut self.stash, bytes) {
            Ok(decoded) => decoded,
            // Not base64: the image is dropped. There is no reply channel to complain through.
            Err(()) => return self.discard(),
        };
        if seq.out.len() + decoded.len() > MAX_PAYLOAD_BYTES {
            return self.discard(); // past the payload cap, and nothing from it is kept
        }
        seq.out.extend_from_slice(&decoded);
        self.seq = Some(seq);
    }

    /// The sequence ended (`inline_image_end`): the image to display, if there is one.
    ///
    /// `None` for a refused or dropped sequence, for a payload with no data, and for a header that
    /// is not PNG, JPEG or GIF, declares a side over [`MAX_IMAGE_SIDE`] or an image whose RGBA8
    /// frame would pass [`MAX_IMAGE_BYTES`], or never reached its size field.
    pub fn end(&mut self) -> Option<InlineImage> {
        let Some(seq) = self.seq.take() else {
            // A stray `end` (`inline_image_abort` already ran, or the argument limit was passed
            // before any start): nothing is in progress and nothing is held.
            self.stash.clear();
            return None;
        };
        let tail = match drain_tail(&mut self.stash) {
            Ok(tail) => tail,
            Err(()) => return None, // the last quad never completed: no bytes to add
        };
        if seq.out.len() + tail.len() > MAX_PAYLOAD_BYTES {
            return None; // the last group put it past the cap
        }
        let mut bytes = seq.out;
        bytes.extend_from_slice(&tail);
        if bytes.is_empty() {
            return None; // a sequence carrying no payload displays nothing
        }
        let (width, height) = header_size(&bytes)?;
        Some(InlineImage {
            bytes,
            pixel_size: (width, height),
            width: seq.args.width,
            height: seq.args.height,
            preserve_aspect: seq.args.preserve_aspect,
            inline: true, // a download never reaches this point
        })
    }

    /// The sequence was cut short (`inline_image_abort`): truncated payload is not an image, so it
    /// is dropped whole. A lone abort with no start is the same thing, and nothing was held for it.
    pub fn abort(&mut self) {
        self.discard();
    }

    /// The sequence in progress is dropped: its bytes are released, later puts are ignored, and
    /// `end` answers nothing.
    fn discard(&mut self) {
        self.seq = None;
        self.stash.clear();
    }

    /// Bytes held for the sequence in progress (test hook).
    #[cfg(test)]
    fn held(&self) -> usize {
        self.seq.as_ref().map_or(0, |seq| seq.out.len()) + self.stash.len()
    }
}

/// What a sequence's arguments asked for, or `None` when it asks for nothing to be displayed.
fn parse_args(args: &[u8]) -> Option<Args> {
    let mut out = Args::new();
    // The vendored parser keeps `;` inside its argument text, so the pairs are split here. A pair
    // without `=`, and an empty one, carry nothing.
    let mut display = false; // `inline=1`, the only reason to keep a sequence at all
    for pair in args.split(|&byte| byte == b';') {
        let Some(eq) = pair.iter().position(|&byte| byte == b'=') else { continue };
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        match key {
            // `name` is base64 text; nothing here shows a filename.
            b"name" => {},
            b"size" => {
                // Advisory byte count: refuse up front what the payload cap could not hold.
                if declared_size_over_cap(value) {
                    return None;
                }
            },
            b"width" => {
                if let Some(width) = dim(value) {
                    out.width = width;
                }
            },
            b"height" => {
                if let Some(height) = dim(value) {
                    out.height = height;
                }
            },
            b"preserveAspectRatio" => match value {
                b"0" => out.preserve_aspect = false,
                b"1" => out.preserve_aspect = true,
                _ => {},
            },
            // `inline=1` asks for display; anything else is a download.
            b"inline" => display = value == b"1",
            // A key this terminal does not act on (`format`, `inband`, …) is skipped.
            _ => {},
        }
    }
    display.then_some(out)
}

/// A `size` value past the payload cap. The count saturates, so an absurd one reads as "too big"
/// and not as "not sent"; anything that is not digits is ignored, being only advisory.
fn declared_size_over_cap(value: &[u8]) -> bool {
    if value.is_empty() || !value.iter().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let size = value
        .iter()
        .fold(0u64, |acc, &digit| acc.saturating_mul(10).saturating_add(u64::from(digit - b'0')));
    size > MAX_PAYLOAD_BYTES as u64
}

/// One `width` or `height` value: `auto`, a cell count, `Npx` or `N%` (the unit's spelling is
/// case-insensitive). Anything else asks for nothing.
fn dim(value: &[u8]) -> Option<Dim> {
    if value.eq_ignore_ascii_case(b"auto") {
        return Some(Dim::Auto);
    }
    if let Some(rest) = value.strip_suffix(b"%") {
        return uint(rest).map(Dim::Percent);
    }
    if value.len() > 2 && value[value.len() - 2..].eq_ignore_ascii_case(b"px") {
        return uint(&value[..value.len() - 2]).map(Dim::Pixels);
    }
    uint(value).map(Dim::Cells)
}

/// A decimal `u32`: plain digits, nothing signed and nothing that does not fit.
fn uint(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// The pixel size of an accepted image, read from its header.
///
/// Only the formats the protocol defines are accepted — PNG, JPEG and GIF — and only within
/// [`MAX_IMAGE_SIDE`] per side and inside the [`MAX_IMAGE_BYTES`] buffer the embedder decodes one
/// RGBA8 frame into. An unknown header, a header whose size field never arrives and a zero side are
/// all "no image".
fn header_size(bytes: &[u8]) -> Option<(u32, u32)> {
    match imagesize::image_type(bytes).ok()? {
        imagesize::ImageType::Png | imagesize::ImageType::Jpeg | imagesize::ImageType::Gif => {},
        _ => return None, // a format the protocol does not define (`tiff`, `webp`, …)
    }
    let size = imagesize::blob_size(bytes).ok()?;
    let (Ok(width), Ok(height)) = (u32::try_from(size.width), u32::try_from(size.height)) else {
        return None; // a header with a side no `u32` can hold
    };
    if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return None;
    }
    if u64::from(width) * u64::from(height) * 4 > MAX_IMAGE_BYTES {
        return None; // one RGBA8 frame is bigger than the embedder has room for
    }
    Some((width, height))
}

// --- payload plumbing ---------------------------------------------------------------------

/// Decode the quad-aligned part of `stash + bytes`; at most three characters (an incomplete quad)
/// stay in `stash` for the next slice. Padding that does not end a quad fails the decode, which
/// drops the whole image: a padded chunk in mid-payload is not base64 the protocol sends.
fn decode_prefix(stash: &mut Vec<u8>, bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let mut all = std::mem::take(stash);
    all.extend_from_slice(bytes);
    let quads = all.len() & !3;
    *stash = all.split_off(quads);
    STANDARD.decode(&all).map_err(|_| ())
}

/// Decode what the stash holds when the payload ends: an incomplete final group, with or without
/// its padding. A single leftover symbol cannot form a byte, and padding with nothing to pad is not
/// data either; either drops the image.
fn drain_tail(stash: &mut Vec<u8>) -> Result<Vec<u8>, ()> {
    let tail = std::mem::take(stash);
    if tail.is_empty() {
        return Ok(Vec::new());
    }
    let Some(at) = tail.iter().position(|&byte| byte != b'=') else {
        return Err(()); // padding with nothing to pad
    };
    let body = &tail[at..];
    if body.len() == 1 {
        return Err(()); // one symbol carries no whole byte
    }
    STANDARD_NO_PAD.decode(body).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};

    use crate::graphics::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE, MAX_PAYLOAD_BYTES};

    /// One complete sequence: `start(args)`, the payload's base64 in a single `put`, `end`.
    fn parse(args: &[u8], payload: &[u8]) -> Option<InlineImage> {
        let mut parser = InlineImageParser::new();
        parser.start(args);
        parser.put(STANDARD.encode(payload).as_bytes());
        parser.end()
    }

    /// The same sequence with the base64 text cut into pieces of `size` bytes.
    fn parse_chunked(args: &[u8], payload: &[u8], size: usize) -> Option<InlineImage> {
        let mut parser = InlineImageParser::new();
        parser.start(args);
        let text = STANDARD.encode(payload).into_bytes();
        for chunk in text.chunks(size) {
            parser.put(chunk);
        }
        parser.end()
    }

    /// A PNG file declaring `w`×`h`. Only its IHDR is read, so the chunk CRC stays zero and no
    /// image data follows.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        // Bit depth, colour type, compression, filter and interlace.
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC: the header reader never looks at it
        v
    }

    /// A JPEG whose baseline SOF0 declares `w`×`h` (again: only the header).
    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.push(3); // sample components; the size reader stops at the width
        v
    }

    /// A GIF89a whose logical screen descriptor declares `w`×`h`.
    fn gif(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&[0x70, 0, 0]); // colour map flags, background, pixel aspect
        v
    }

    /// A BMP (BITMAPINFOHEADER) declaring `w`×`h`: a format `imagesize` knows and the protocol does
    /// not define.
    fn bmp(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"BM".to_vec();
        v.extend_from_slice(&54u32.to_le_bytes()); // file size
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved
        v.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        v.extend_from_slice(&40u32.to_le_bytes()); // DIB header size
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    /// A WebP VP8X canvas declaring `w`×`h`, the other format imagesize knows and we refuse.
    fn webp(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&40u32.to_le_bytes()); // container size
        v.extend_from_slice(b"WEBPVP8X");
        v.extend_from_slice(&10u32.to_le_bytes()); // VP8X chunk size
        v.extend_from_slice(&[0, 0, 0, 0]); // flags and reserved
        v.extend_from_slice(&[(w - 1) as u8, ((w - 1) >> 8) as u8, 0]); // canvas width less one
        v.extend_from_slice(&[(h - 1) as u8, ((h - 1) >> 8) as u8, 0]); // canvas height less one
        v
    }

    /// `data` with `pad` bytes appended: payload the parser never looks at, used to make base64
    /// that needs one, two or no padding characters.
    fn padded(data: &[u8], pad: usize) -> Vec<u8> {
        let mut v = data.to_vec();
        v.resize(v.len() + pad, b'x');
        v
    }

    #[test]
    fn parses_args() {
        // `name` is base64 text and is read and dropped; `size` is advisory; `width`, `height`,
        // `preserveAspectRatio` and `inline` are what the embedder acts on.
        let data = png(21, 9);
        let args = format!(
            "name=YW5pbWFsLnBuZw==;size={};width=12;height=64px;preserveAspectRatio=0;inline=1",
            data.len()
        );
        let image = parse(args.as_bytes(), &data).expect("an inline image");
        assert_eq!(image.bytes, data);
        assert_eq!(image.pixel_size, (21, 9));
        assert_eq!(image.width, Dim::Cells(12));
        assert_eq!(image.height, Dim::Pixels(64));
        assert!(!image.preserve_aspect);
        assert!(image.inline);

        // Keys this terminal does not act on are skipped, and a value that is not one of the
        // defined forms leaves its key at its default. `format` claims TIFF while the payload is
        // PNG: the header decides, as it always does.
        let image = parse(b"inline=1;format=tiff;inband=1;width=nonsense;size=abc;junk", &data)
            .expect("unknown arguments are skipped");
        assert_eq!(image.width, Dim::Auto);
        assert_eq!(image.pixel_size, (21, 9));
        assert!(image.preserve_aspect);

        // A declared size past the payload cap is refused before a byte of payload arrives; one
        // that fits the cap exactly is not.
        let over = format!("inline=1;size={}", MAX_PAYLOAD_BYTES + 1);
        assert!(parse(over.as_bytes(), &data).is_none());
        let exact = format!("inline=1;size={MAX_PAYLOAD_BYTES}");
        assert!(parse(exact.as_bytes(), &data).is_some());

        // A key sent twice keeps its last value.
        let image = parse(b"inline=1;preserveAspectRatio=0;preserveAspectRatio=1", &data)
            .expect("the last value wins");
        assert!(image.preserve_aspect);
        assert!(parse(b"inline=1;inline=0", &data).is_none());

        // A sequence with no payload displays nothing.
        assert!(parse(b"inline=1", b"").is_none());
    }

    #[test]
    fn dims_in_all_units() {
        let data = png(4, 4);
        let check = |args: &str, width: Dim, height: Dim| {
            let image = parse(format!("inline=1;{args}").as_bytes(), &data).expect(args);
            assert_eq!(image.width, width, "{args}");
            assert_eq!(image.height, height, "{args}");
        };
        check("width=3;height=7", Dim::Cells(3), Dim::Cells(7));
        check("width=80px;height=24px", Dim::Pixels(80), Dim::Pixels(24));
        check("width=50%;height=100%", Dim::Percent(50), Dim::Percent(100));
        check("width=auto;height=AUTO", Dim::Auto, Dim::Auto);
        check("width=40;height=30%", Dim::Cells(40), Dim::Percent(30));
        check("height=24PX", Dim::Auto, Dim::Pixels(24));
        // Absent, unreadable or too large for `u32`: the image's own pixels.
        check("", Dim::Auto, Dim::Auto);
        check("width=%;height=12pxx", Dim::Auto, Dim::Auto);
        check("width=4294967296;height=-3", Dim::Auto, Dim::Auto);
    }

    #[test]
    fn download_is_discarded() {
        let data = png(4, 4);
        // `inline=0` downloads the file without displaying it, and an absent `inline` asks for the
        // same thing: this fork displays images and does nothing else.
        assert!(parse(b"size=33;inline=0;width=10", &data).is_none());
        assert!(parse(b"", &data).is_none());
        assert!(parse(b"name=x.png", &data).is_none());

        // A discarded sequence stores nothing while its payload streams past it.
        let mut parser = InlineImageParser::new();
        parser.start(b"inline=0");
        let text = STANDARD.encode(vec![b'x'; 1 << 20]).into_bytes();
        for chunk in text.chunks(997) {
            parser.put(chunk);
            assert_eq!(parser.held(), 0);
        }
        assert!(parser.end().is_none());

        // The next sequence is unaffected by the discarded one.
        parser.start(b"inline=1");
        parser.put(STANDARD.encode(&data).as_bytes());
        assert_eq!(parser.end().expect("the next image").pixel_size, (4, 4));
    }

    #[test]
    fn base64_across_puts() {
        // Every payload length modulo three, so padded and unpadded base64 are both covered.
        for pad in 0..3 {
            let data = padded(&png(2, 3), pad);
            let whole = parse(b"inline=1", &data).expect("the payload in one put");
            assert_eq!(whole.pixel_size, (2, 3));
            for size in [1, 2, 3, 4, 5, 7] {
                let chunked = parse_chunked(b"inline=1", &data, size).expect("chunked payload");
                assert_eq!(chunked.bytes, whole.bytes, "chunks of {size}, pad {pad}");
            }

            // An unpadded tail decodes to the same bytes.
            let mut parser = InlineImageParser::new();
            parser.start(b"inline=1");
            parser.put(STANDARD_NO_PAD.encode(&data).as_bytes());
            assert_eq!(parser.end().expect("unpadded base64").bytes, data);

            // Every split of the text into two puts gives the same image.
            let text = STANDARD.encode(&data).into_bytes();
            for at in 0..=text.len() {
                let mut parser = InlineImageParser::new();
                parser.start(b"inline=1");
                parser.put(&text[..at]);
                parser.put(&text[at..]);
                assert_eq!(parser.end().expect("split at {at}").bytes, data);
            }
        }

        // Only an incomplete quad is carried between puts: fed one character at a time, the parser
        // never holds more than three characters for the next quad.
        let data = png(2, 3);
        let mut parser = InlineImageParser::new();
        parser.start(b"inline=1");
        for byte in STANDARD.encode(&data).into_bytes() {
            parser.put(&[byte]);
            assert!(parser.stash.len() <= 3, "at most an incomplete quad is carried over");
        }
        assert_eq!(parser.end().expect("character by character").bytes, data);
    }

    #[test]
    fn oversized_is_dropped() {
        // Sides and decoded bytes are checked against the shared limits before anything else; only
        // the header is needed to declare them. The byte cap is what the embedder decodes one frame
        // into, so 4096×4096 RGBA8 — exactly MAX_IMAGE_BYTES — is the boundary.
        assert_eq!(4096u64 * 4096 * 4, MAX_IMAGE_BYTES);
        assert!(parse(b"inline=1", &png(MAX_IMAGE_SIDE + 1, 1)).is_none());
        assert!(parse(b"inline=1", &png(1, MAX_IMAGE_SIDE + 1)).is_none());
        assert_eq!(
            parse(b"inline=1", &png(4096, 4096))
                .expect("exactly MAX_IMAGE_BYTES of RGBA")
                .pixel_size,
            (4096, 4096)
        );
        assert!(parse(b"inline=1", &png(4097, 4096)).is_none()); // one row past the byte cap
        assert_eq!(
            parse(b"inline=1", &gif(u16::try_from(MAX_IMAGE_SIDE).unwrap(), 1))
                .expect("the side cap holds")
                .pixel_size,
            (MAX_IMAGE_SIDE, 1)
        );

        // A payload is capped at MAX_PAYLOAD_BYTES decoded: the cap itself fits, one byte more does
        // not.
        let header = png(1, 1);
        let slack = MAX_PAYLOAD_BYTES - header.len();
        assert!(parse(b"inline=1", &padded(&header, slack)).is_some());
        assert!(parse(b"inline=1", &padded(&header, slack + 1)).is_none());

        // Past the cap mid-payload the bytes are released at once, not held until `end`, and they
        // never pass the cap on the way there.
        let mut parser = InlineImageParser::new();
        parser.start(b"inline=1");
        parser.put(STANDARD.encode(&header).as_bytes()); // a real header: only the cap can drop this
        let chunk = STANDARD.encode(vec![b'x'; 3 * 1024]).into_bytes(); // 1 KiB of payload per put
        let mut dropped = false;
        for _ in 0..=MAX_PAYLOAD_BYTES / 1024 {
            parser.put(&chunk);
            if parser.held() == 0 {
                dropped = true;
                break;
            }
            assert!(parser.held() <= MAX_PAYLOAD_BYTES);
        }
        assert!(dropped, "the sequence is dropped as soon as it passes the cap");
        assert_eq!(parser.held(), 0);
        assert!(parser.end().is_none());

        // And a header over the image caps drops the sequence at `end`, releasing its payload.
        let mut parser = InlineImageParser::new();
        parser.start(b"inline=1");
        parser.put(STANDARD.encode(png(MAX_IMAGE_SIDE + 1, MAX_IMAGE_SIDE + 1)).as_bytes());
        assert!(parser.end().is_none());
        assert_eq!(parser.held(), 0);
    }

    #[test]
    fn unknown_format_is_dropped() {
        // The three formats the protocol defines are accepted, with the size from their headers.
        assert_eq!(parse(b"inline=1", &png(7, 5)).expect("png").pixel_size, (7, 5));
        assert_eq!(parse(b"inline=1", &jpeg(7, 5)).expect("jpeg").pixel_size, (7, 5));
        assert_eq!(parse(b"inline=1", &gif(7, 5)).expect("gif").pixel_size, (7, 5));

        // A format the header reader knows but the protocol does not define is refused even when
        // its size reads fine.
        for bytes in [bmp(3, 2), webp(3, 2)] {
            assert!(imagesize::blob_size(&bytes).is_ok(), "fixture should carry a size");
            assert!(parse(b"inline=1", &bytes).is_none());
        }

        // Data with no header, and a header cut short before its size, are no image either.
        assert!(parse(b"inline=1", b"not an image at all").is_none());
        assert!(parse(b"inline=1", &png(7, 5)[..20]).is_none());
    }

    #[test]
    fn abort_drops_the_sequence() {
        // A lone abort (the argument limit passed before any start) is a dropped image, not an
        // unfinished one.
        let mut parser = InlineImageParser::new();
        parser.abort();
        assert!(parser.end().is_none());
        parser.put(b"AAAA"); // a stray put with no sequence in progress is consumed
        assert_eq!(parser.held(), 0);

        // An aborted payload is truncated data, not an image: it is dropped and released.
        parser.start(b"inline=1");
        let text = STANDARD.encode(png(2, 2)).into_bytes();
        parser.put(&text[..3]); // mid-quad
        assert_eq!(parser.held(), 3);
        parser.abort();
        assert_eq!(parser.held(), 0);
        assert!(parser.end().is_none());

        // The next sequence parses normally.
        parser.start(b"inline=1");
        parser.put(&text);
        assert_eq!(parser.end().expect("the next image").pixel_size, (2, 2));

        // A sequence started without ending is discarded by the next start.
        let mut parser = InlineImageParser::new();
        parser.start(b"inline=1");
        parser.put(STANDARD.encode(png(9, 9)).as_bytes());
        parser.start(b"inline=1;width=2");
        assert_eq!(parser.held(), 0);
        parser.put(&text);
        let image = parser.end().expect("the second sequence");
        assert_eq!(image.pixel_size, (2, 2));
        assert_eq!(image.width, Dim::Cells(2));
    }

    #[test]
    fn padding_must_end_the_payload() {
        // A sender that pads every chunk of one blob is not sending the base64 the protocol
        // defines: the image is dropped rather than guessed at.
        let data = png(2, 2);
        let mut text = STANDARD.encode(&data[..16]); // padded in the middle of the payload
        text.push_str(&STANDARD.encode(&data[16..]));
        let mut parser = InlineImageParser::new();
        parser.start(b"inline=1");
        parser.put(text.as_bytes());
        assert!(parser.end().is_none());
        assert_eq!(parser.held(), 0);

        // Padding at the end of the payload is normal, and so is none at all: `base64_across_puts`
        // covers both.
        assert!(parse(b"inline=1", &data).is_some());
    }
}
