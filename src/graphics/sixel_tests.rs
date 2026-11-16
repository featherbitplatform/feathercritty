// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Tests for the sixel decoder (`sixel.rs`).
//!
//! The three reference images under `tests/fixtures/sixel/` are written by [`write_fixtures`] (run
//! it with `cargo test --lib graphics::sixel_tests::write_fixtures -- --ignored`): a small encoder
//! in this file renders each fixture's command script to sixel bytes, and the matching `.png` is
//! that fixture's expected pixels, written straight from a literal grid (the `png` crate is a
//! dev-dependency for exactly this round trip). The fixtures hold only the bytes between the `q`
//! of the DCS and its terminator — what the parser streams to `put` — so each one also records the
//! `params` and background colour the decoder must be constructed with.

use std::fs;
use std::io::Cursor;

use super::limits::*;
use super::sixel::*;

/// The directory of the reference fixtures (committed; produced by `write_fixtures`).
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sixel");

// Colours the fixtures and unit tests paint with, spelled out so expectations read directly.
const BG: Rgba = Rgba::new(10, 20, 30);
const RED: Rgba = Rgba::new(255, 0, 0); // `#..;2;100;0;0`
const GREEN: Rgba = Rgba::new(0, 255, 0); // `#..;2;0;100;0`
const BLUE: Rgba = Rgba::new(0, 0, 255); // `#..;2;0;0;100`
const YELLOW: Rgba = Rgba::new(255, 255, 0); // `#..;2;100;100;0`
const VIOLET: Rgba = Rgba::new(128, 0, 255); // `#..;2;50;0;100`
const CLEAR: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };
/// The VT340 default palette's register 1 (blue) and 2 (red), used through `#1`/`#2`.
const PAL_BLUE: [u8; 4] = [51, 51, 201, 255];
const PAL_RED: [u8; 4] = [201, 33, 33, 255];

/// A column of six rows: the colour painted at each row, or `None` for "not written here" —
/// expectations are columns because that is what a sixel is. In an opaque fixture a zero bit *is*
/// written (with the background), so its column says `Some(BG)`; only a transparent fixture uses
/// plain `None`.
type Col = [Option<Rgba>; 6];

/// A column whose lit rows all carry one colour and whose zero rows take `bg` (`Some(..)`, e.g. the
/// background of an opaque image) or are left alone (`None`, a transparent image).
const fn col_with(rows: [bool; 6], color: Rgba, bg: Option<Rgba>) -> Col {
    let mut c = [None; 6];
    let mut r = 0;
    while r < 6 {
        c[r] = if rows[r] { Some(color) } else { bg };
        r += 1;
    }
    c
}

const T_ROWS: [bool; 6] = [true, false, true, false, true, true]; // `t` (0x35): bits 0, 2, 4, 5
const I_ROWS: [bool; 6] = [false, true, false, true, false, true]; // `i` (0x69): rows 1, 3, 5
const ALL_ROWS: [bool; 6] = [true; 6]; // `~`
const NO_ROWS: [bool; 6] = [false; 6]; // `?`

/// One command of a fixture script; [`encode`] renders these the way a sixel encoder would.
#[derive(Clone, Copy)]
enum Op {
    /// `"0;1;Ph;Pv`: declare the image size (the aspect pair is fixed at 0;1 here).
    Raster { width: u32, height: u32 },
    /// `#reg;2;r;g;b` defining and selecting a register.
    Def { reg: u32, rgb: (u32, u32, u32) },
    /// A sixel character value painted `n` times with the current colour (`value & 1 << row` lit);
    /// runs go out in the protocol's own `!n` form.
    Run { value: u8, n: u32 },
    /// Graphics carriage return.
    CR,
    /// Graphics newline.
    NL,
}

/// Render a script to sixel data bytes.
fn encode(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        match *op {
            Op::Raster { width, height } => {
                out.extend_from_slice(format!("\"0;1;{width};{height}").as_bytes())
            },
            Op::Def { reg, rgb: (r, g, b) } => {
                out.extend_from_slice(format!("#{reg};2;{r};{g};{b}").as_bytes())
            },
            Op::Run { value, n } => {
                if n > 1 {
                    out.extend_from_slice(format!("!{n}").as_bytes());
                }
                out.push(b'?' + value);
            },
            Op::CR => out.push(b'$'),
            Op::NL => out.push(b'-'),
        }
    }
    out
}

/// One fixture: the command script, its parameters and background, and its expected pixels.
struct Fixture {
    name: &'static str,
    params: Vec<u16>,
    bg: Rgba,
    ops: Vec<Op>,
    width: u32,
    height: u32,
    /// Expected pixels as `width * height * 4` RGBA8, row-major.
    rgba: Vec<u8>,
}

/// Build an expected grid from bands of columns: `bands` are top-to-bottom groups of six rows of
/// `width` columns; a row past `height` is truncated (a declared size can be taller than the data),
/// and any pixel a column does not paint gets `untouched`.
fn grid(width: u32, height: u32, untouched: Rgba, bands: &[&[Col]]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for (band, columns) in bands.iter().enumerate() {
        assert_eq!(columns.len(), width as usize, "a band spans the full width");
        for r in 0..6u32 {
            let y = band as u32 * 6 + r;
            if y >= height {
                break;
            }
            for column in *columns {
                let color = column[r as usize].unwrap_or(untouched);
                rgba.extend_from_slice(&[color.r, color.g, color.b, color.a]);
            }
        }
    }
    assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4, "fixture grid is complete");
    rgba
}

/// The three reference images.
fn fixtures() -> Vec<Fixture> {
    vec![basic_columns(), overprint_and_newline(), transparent_declared()]
}

/// `basic_columns.six`: one band of eight columns with no raster attributes — mixed bit patterns, a
/// full column, an all-zero column (the background colour, since `P2` is 0 here), a single lit row,
/// and a repeat run. The canvas grows from the data alone.
fn basic_columns() -> Fixture {
    let ops = vec![
        Op::Def { reg: 2, rgb: (100, 0, 0) },
        Op::Run { value: 0x35, n: 1 }, // `t`: red rows 0, 2, 4, 5
        Op::Def { reg: 3, rgb: (0, 100, 0) },
        Op::Run { value: 0x3F, n: 1 }, // `~`: all green
        Op::Run { value: 0, n: 1 },    // `?`: all background
        Op::Def { reg: 4, rgb: (0, 0, 100) },
        Op::Run { value: 2, n: 1 },    // `A`: row 1 blue
        Op::Run { value: 0x35, n: 1 }, // `t` again — still register 4, so blue, not red
        Op::Def { reg: 3, rgb: (0, 100, 0) },
        Op::Run { value: 0x3F, n: 3 }, // `!3~`: three green columns
    ];
    let rgba = grid(8, 6, BG, &[&[
        col_with(T_ROWS, RED, Some(BG)),
        col_with(ALL_ROWS, GREEN, Some(BG)),
        col_with(NO_ROWS, GREEN, Some(BG)),
        col_with([false, true, false, false, false, false], BLUE, Some(BG)),
        col_with(T_ROWS, BLUE, Some(BG)),
        col_with(ALL_ROWS, GREEN, Some(BG)),
        col_with(ALL_ROWS, GREEN, Some(BG)),
        col_with(ALL_ROWS, GREEN, Some(BG)),
    ]]);
    Fixture { name: "basic_columns", params: vec![], bg: BG, ops, width: 8, height: 6, rgba }
}

/// `overprint_and_newline.six`: `"0;1;8;12` declares 8x12, which an opaque image erases to the
/// background colour; a red band, `$` back to column 0 and green overprint whose *zero* bits
/// repaint the background — erasing the red it covers; `-` to a second band with one blue pixel.
/// Only rows 0-6 are ever painted: the declared size is what makes the image 12 rows tall.
fn overprint_and_newline() -> Fixture {
    let ops = vec![
        Op::Raster { width: 8, height: 12 },
        Op::Def { reg: 2, rgb: (100, 0, 0) },
        Op::Run { value: 0x35, n: 3 }, // `!3t`: red rows 0, 2, 4, 5 in columns 0-2
        Op::Run { value: 0, n: 1 },    // `?` column 3: background
        Op::CR,
        Op::Def { reg: 3, rgb: (0, 100, 0) },
        Op::Run { value: 42, n: 3 }, // `!3i`: green rows 1, 3, 5 in columns 0-2; zero bits erase
        Op::NL,
        Op::Def { reg: 4, rgb: (0, 0, 100) },
        Op::Run { value: 1, n: 1 }, // `@` column 0 of the second band: absolute row 6 blue
    ];
    let bg_col = col_with(NO_ROWS, BG, Some(BG));
    let rgba = grid(8, 12, BG, &[
        // Band 0: columns 0-2 hold only the green overprint — its zero bits repainted rows 0, 2, 4
        // with the background, so no red survived. Columns 3-7 were never painted by anything
        // else.
        &[
            col_with(I_ROWS, GREEN, Some(BG)),
            col_with(I_ROWS, GREEN, Some(BG)),
            col_with(I_ROWS, GREEN, Some(BG)),
            bg_col,
            bg_col,
            bg_col,
            bg_col,
            bg_col,
        ],
        // Band 1: one blue pixel at row 6 of column 0; the rest is declared background.
        &[
            col_with([true, false, false, false, false, false], BLUE, Some(BG)),
            bg_col,
            bg_col,
            bg_col,
            bg_col,
            bg_col,
            bg_col,
            bg_col,
        ],
    ]);
    Fixture {
        name: "overprint_and_newline",
        params: vec![],
        bg: BG,
        ops,
        width: 8,
        height: 12,
        rgba,
    }
}

/// `transparent_declared.six`: `P2=1` with a declared 6x7 canvas — never-painted pixels are alpha
/// 0, and the overprint after `$` *leaves* the yellow its zero bits pass over, which is exactly
/// where an opaque image would have erased it (see `overprint_and_newline`). The background colour
/// handed to the decoder appears nowhere in the image.
fn transparent_declared() -> Fixture {
    let ops = vec![
        Op::Raster { width: 6, height: 7 },
        Op::Def { reg: 2, rgb: (100, 100, 0) },
        Op::Run { value: 0x3F, n: 3 }, // `!3~`: yellow columns 0-2
        Op::CR,
        Op::Def { reg: 3, rgb: (50, 0, 100) },
        Op::Run { value: 42, n: 1 }, // `i`: violet rows 1, 3, 5 of column 0; yellow survives
    ];
    let clear_col = col_with(NO_ROWS, CLEAR, None);
    let rgba = grid(6, 7, CLEAR, &[
        &[
            [Some(YELLOW), Some(VIOLET), Some(YELLOW), Some(VIOLET), Some(YELLOW), Some(VIOLET)],
            col_with(ALL_ROWS, YELLOW, None),
            col_with(ALL_ROWS, YELLOW, None),
            clear_col,
            clear_col,
            clear_col,
        ],
        // Row 6 is declared but never painted: transparent. Rows 7-11 of this band fall off the
        // bottom of a 7-row image.
        &[clear_col; 6],
    ]);
    Fixture {
        name: "transparent_declared",
        params: vec![0, 1],
        bg: BG,
        ops,
        width: 6,
        height: 7,
        rgba,
    }
}

/// Decode with the parameters and background a stream was authored for; must produce an image.
fn decode(data: &[u8], params: &[u16], bg: Rgba) -> SixelImage {
    let mut decoder = SixelDecoder::new(params, bg);
    decoder.put(data);
    decoder.finish().expect("stream decodes to an image")
}

/// Decode expecting the zero-size error instead.
fn decode_empty(data: &[u8], params: &[u16], bg: Rgba) -> SixelError {
    let mut decoder = SixelDecoder::new(params, bg);
    decoder.put(data);
    decoder.finish().expect_err("zero-sized image is an error")
}

/// The four bytes of one pixel.
fn px(image: &SixelImage, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * image.width + x) as usize) * 4;
    image.rgba[i..i + 4].try_into().expect("pixel inside the buffer")
}

/// Read a committed fixture file, pointing at the generator when it is missing.
fn fixture_file(name: &str, ext: &str) -> Vec<u8> {
    let path = format!("{FIXTURE_DIR}/{name}.{ext}");
    fs::read(&path)
        .unwrap_or_else(|e| panic!("read {path}: {e} (run write_fixtures and commit it)"))
}

/// The expected pixels of a PNG fixture, decoded with the `png` crate.
fn png_pixels(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("fixture PNG header");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("fixture PNG pixels");
    assert_eq!(info.color_type, png::ColorType::Rgba, "fixtures are RGBA8");
    (info.width, info.height, buf)
}

/// Write the fixtures: sixel bytes from [`encode`], expected pixels straight to PNG. It checks its
/// own output through the decoder before committing anything to disk, so a fixture whose script and
/// expected grid disagree fails here rather than silently shipping wrong files.
#[test]
#[ignore]
fn write_fixtures() {
    for f in fixtures() {
        let data = encode(&f.ops);
        let image = decode(&data, &f.params, f.bg);
        assert_eq!((image.width, image.height), (f.width, f.height), "{} size", f.name);
        assert_eq!(image.rgba, f.rgba, "{} pixels", f.name);
    }
    fs::create_dir_all(FIXTURE_DIR).expect("fixture directory exists");
    for f in fixtures() {
        fs::write(format!("{FIXTURE_DIR}/{}.six", f.name), encode(&f.ops))
            .expect("write sixel fixture");
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, f.width, f.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_filter(png::FilterType::NoFilter);
            encoder
                .write_header()
                .expect("PNG header")
                .write_image_data(&f.rgba)
                .expect("PNG pixels");
        }
        fs::write(format!("{FIXTURE_DIR}/{}.png", f.name), &png).expect("write PNG fixture");
    }
}

/// The three committed fixtures decode, through the real `put`/`finish`, to exactly the pixels of
/// their committed PNGs: bit order (the least significant bit is the top row), RGB colour
/// introducers, what a zero bit paints with an opaque image, `$` overprint, `-` bands, `!` repeats
/// and declared sizes.
#[test]
fn decodes_reference_images() {
    for f in fixtures() {
        let data = fixture_file(f.name, "six");
        let (w, h, pixels) = png_pixels(&fixture_file(f.name, "png"));
        let image = decode(&data, &f.params, f.bg);
        assert_eq!((image.width, image.height), (w, h), "{} size", f.name);
        assert_eq!(image.rgba, pixels, "{} pixels", f.name);
    }
}

/// `#Pc;1;H;L;S` converts HLS to RGB: the pure hues at full saturation and half lightness, greys at
/// saturation 0 (lightness only), clamping of out-of-range coordinates, that a definition also
/// selects what it defined — and one RGB introducer with a clamped channel in the same stream.
#[test]
fn hls_colours() {
    let defs: &[&str] = &[
        "#16;1;0;50;100",    // red (hue 0 is red on the ordinary colour wheel)
        "#17;1;120;50;100",  // green
        "#18;1;240;50;100",  // blue
        "#19;1;60;50;100",   // yellow
        "#20;1;0;0;0",       // black: saturation 0 means lightness only
        "#21;1;0;100;0",     // white
        "#22;1;0;40;0",      // grey, lightness 40 -> 102 per channel
        "#23;1;400;150;150", // hue wraps (400 is 40), lightness and saturation clamp: white
        "#24;2;150;0;50",    // RGB with a clamped channel: 255, 0, 128
    ];
    let mut stream = Vec::new();
    for def in defs {
        stream.extend_from_slice(def.as_bytes());
        stream.push(b'~');
    }
    let image = decode(&stream, &[], BG);
    assert_eq!((image.width, image.height), (9, 6));
    let want = [
        RED,
        GREEN,
        BLUE,
        YELLOW,
        Rgba::new(0, 0, 0),
        Rgba::new(255, 255, 255),
        Rgba::new(102, 102, 102),
        Rgba::new(255, 255, 255),
        Rgba::new(255, 0, 128), // `#24;2;150;0;50`: red clamps to 100 percent
    ];
    for (x, &color) in want.iter().enumerate() {
        for y in 0..6u32 {
            assert_eq!(
                px(&image, x as u32, y),
                [color.r, color.g, color.b, 255],
                "column {x} row {y}"
            );
        }
    }
}

/// `!Pn` repeats the next character (an omitted count repeats once), `$` returns to column 0 of the
/// band, `-` starts the next band and grows the canvas downward; with `P2=1` a later pass covers an
/// earlier one only where its bits are set.
#[test]
fn repeat_and_newline() {
    // Transparent: five blue columns, `$`, three red over the first three (full-height sixels, so
    // the second pass wins exactly in columns 0-2).
    let image = decode(b"#1!5~$#2!3~", &[0, 1], BG);
    assert_eq!((image.width, image.height), (5, 6));
    for x in 0..5u32 {
        for y in 0..6u32 {
            let want = if x < 3 { PAL_RED } else { PAL_BLUE };
            assert_eq!(px(&image, x, y), want, "column {x} row {y}");
        }
    }

    // Newline growth: two full blue columns in band 0, then a new band with three columns lit only
    // in its first row (absolute row 6): the canvas is 3 wide and 12 tall, everything else
    // transparent.
    let image = decode(b"#1!2~-!3@", &[0, 1], BG);
    assert_eq!((image.width, image.height), (3, 12));
    for x in 0..3u32 {
        for y in 0..12u32 {
            let want = match (x, y) {
                (0 | 1, 0..=5) => PAL_BLUE,
                (0..=2, 6) => PAL_BLUE,
                _ => [0, 0, 0, 0],
            };
            assert_eq!(px(&image, x, y), want, "column {x} row {y}");
        }
    }

    // An omitted count is one copy: `!~` equals a plain `~`.
    assert_eq!(decode(b"#1!~", &[0, 1], BG), decode(b"#1~", &[0, 1], BG));
}

/// With `P2=1` never-painted pixels are transparent: the declared area is alpha 0, a `?` column
/// leaves nothing behind it, and the background colour handed to the decoder appears nowhere.
#[test]
fn transparent_background() {
    let image = decode(b"\"0;1;4;6#2~?", &[0, 1], Rgba::new(7, 8, 9));
    assert_eq!((image.width, image.height), (4, 6));
    for x in 0..4u32 {
        for y in 0..6u32 {
            let want = if x == 0 { PAL_RED } else { [0, 0, 0, 0] };
            assert_eq!(px(&image, x, y), want, "column {x} row {y}");
        }
    }
    assert!(
        image.rgba.chunks(4).all(|p| p != [7, 8, 9, 255].as_slice()),
        "the background colour is never painted"
    );
}

/// Each image gets its own 1024 registers: all of them are definable and usable; a definition past
/// the last register neither defines nor selects; an undefined register leaves nothing to paint
/// with; `#Pc` alone re-selects a defined one; and definitions never leak into the next image.
#[test]
fn register_limit() {
    // Define registers 16..=1023 with distinct RGB values and paint one column each.
    let mut stream = Vec::new();
    let mut want: Vec<[u8; 4]> = Vec::new();
    for reg in 16..1024u32 {
        let (r, g, b) = ((reg * 7) % 101, (reg * 13) % 101, (reg * 29) % 101);
        stream.extend_from_slice(format!("#{reg};2;{r};{g};{b}").as_bytes());
        stream.push(b'~');
        want.push([
            ((r * 255 + 50) / 100) as u8,
            ((g * 255 + 50) / 100) as u8,
            ((b * 255 + 50) / 100) as u8,
            255,
        ]);
    }
    let image = decode(&stream, &[], BG);
    assert_eq!((image.width, image.height), (1008, 6));
    for x in 0..1008u32 {
        for y in 0..6u32 {
            assert_eq!(px(&image, x, y), want[x as usize], "column {x} row {y}");
        }
    }

    // A definition past the last register changes nothing: it defines and selects nothing, so the
    // `$` and full sixel after it repaint column 0 with register 1023's colour.
    let mut more = stream.clone();
    more.extend_from_slice(b"#1024;2;50;50;50$~");
    let image = decode(&more, &[], BG);
    assert_eq!((image.width, image.height), (1008, 6));
    for y in 0..6u32 {
        assert_eq!(px(&image, 0, y), want[1007], "row {y} column 0 keeps register 1023");
    }

    // An undefined register selects nothing to paint with: a full sixel paints nothing and does not
    // even grow the canvas (every row is lit, so no background pixel is written either).
    let mut decoder = SixelDecoder::new(&[], BG);
    decoder.put(b"#1!5~");
    decoder.put(b"#700~$#1023~");
    let image = decoder.finish().expect("register 1 painted something");
    assert_eq!((image.width, image.height), (5, 6));
    for x in 0..5u32 {
        for y in 0..6u32 {
            assert_eq!(px(&image, x, y), PAL_BLUE, "column {x} row {y}");
        }
    }

    // Private to one image: a fresh decoder has the VT340 defaults again, and register 16 is
    // undefined until some `#16;...` programs it.
    assert_eq!(decode_empty(b"#16~", &[], BG), SixelError::Empty);
}

/// An endless `!32767~` stream: the canvas stops at the caps, later data is ignored without another
/// allocation, `finish` returns what fits, and a stream that ends mid-number still decodes.
#[test]
fn unterminated_sixel_is_bounded() {
    let mut decoder = SixelDecoder::new(&[], BG);
    decoder.put(b"#1"); // select a colour so the full sixels actually paint
    for _ in 0..50 {
        decoder.put(b"!32767~");
    }
    assert_eq!(decoder.canvas(), (MAX_IMAGE_SIDE, 6, (MAX_IMAGE_SIDE as usize) * 6 * 4));

    // Keep adding bands: the height grows until the byte cap binds, then stops. Earlier pixels are
    // kept, later ones dropped; nothing shrinks and nothing passes a cap.
    for _ in 0..500 {
        decoder.put(b"$-!32767~");
    }
    let (w, h, len) = decoder.canvas();
    assert_eq!(w, MAX_IMAGE_SIDE);
    assert!(h <= MAX_IMAGE_SIDE && h > 6, "height grew and stopped: {h}");
    assert_eq!(len, (w as usize) * (h as usize) * 4);
    assert!((w as u64) * (h as u64) * 4 <= MAX_IMAGE_BYTES, "byte cap holds at {w}x{h}");
    let capped = decoder.canvas();
    for _ in 0..500 {
        decoder.put(b"$-!32767~");
    }
    assert_eq!(decoder.canvas(), capped, "a capped canvas stays capped");

    // A number cut off by the end of the stream commits as it stands, and a repeat without its
    // character paints nothing: either way the image is exactly as far along as it got.
    decoder.put(b"!327");
    let image = decoder.finish().expect("returns what fits");
    assert_eq!((image.width, image.height), (w, h));
    assert_eq!(image.rgba.len(), len);
}

/// A stream that paints nothing and declares no size has zero pixels and is an error: empty data, a
/// transparent run of `?` columns (nothing written at all), and a declared 0x0.
#[test]
fn zero_size_is_error() {
    assert_eq!(SixelDecoder::new(&[], BG).finish(), Err(SixelError::Empty));
    assert_eq!(decode_empty(b"?!?!?", &[0, 1], BG), SixelError::Empty);
    assert_eq!(decode_empty(b"\"0;0;0;0", &[], BG), SixelError::Empty);
}

/// The state machine resumes exactly where a `put` left it: every fixture decodes identically fed
/// whole, one byte at a time, or in chunks — including splits inside numbers, colour introducers
/// and raster attributes.
#[test]
fn split_across_puts_is_identical() {
    for f in fixtures() {
        let data = encode(&f.ops);
        let want = decode(&data, &f.params, f.bg);
        for chunk in [1usize, 2, 3, 7, 11] {
            let mut decoder = SixelDecoder::new(&f.params, f.bg);
            for part in data.chunks(chunk) {
                decoder.put(part);
            }
            assert_eq!(
                decoder.finish().expect("chunked decode"),
                want,
                "{} at chunk {chunk}",
                f.name
            );
        }
    }
}
