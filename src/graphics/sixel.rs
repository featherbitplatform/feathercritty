// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The sixel graphics decoder (feathercritty).
//!
//! [`SixelDecoder`] is fed by the sixel callbacks of [`crate::vte`]: `sixel_start(params)` calls
//! [`new`](SixelDecoder::new), each `sixel_put` a [`put`](SixelDecoder::put), and both `sixel_end`
//! and `sixel_abort` a [`finish`](SixelDecoder::finish) — the decoder does not distinguish them,
//! a truncated stream simply decodes as far as it got. What reaches `put` is exactly the bytes
//! between the `q` of the DCS and its terminator; the parameters in front of the `q` arrive once,
//! through `new`.
//!
//! What this decoder understands follows the DEC VT330/VT340 graphics definition (Volume 2,
//! Chapter 14): sixel data characters `?` (0x3F) to `~` (0x7E), each encoding six vertical pixels
//! whose least significant bit is the top pixel; the graphics repeat introducer `!Pn` with a
//! decimal count; graphics carriage return `$` and graphics newline `-`; the set raster attributes
//! command `"Pan;Pad;Ph;Pv`, whose `Ph`/`Pv` declare the image's pixel size (`Pan`/`Pad` describe
//! the pixel shape, which is a display concern and does not change these pixels); and the colour
//! introducer `#Pc` alone (select a register) or as `#Pc;Pu;Px;Py;Pz` defining one — `Pu=2` gives
//! RGB with each channel 0-100, `Pu=1` gives HLS with hue 0-360 and lightness and saturation
//! 0-100. A register definition also selects the register. Every image gets its own private set of
//! [`REGISTERS`] colour registers; 0 to 15 start as the VT340 factory-default colour map, the rest
//! start undefined. `P2=1` (the second DCS parameter) makes never-painted pixels transparent; with
//! `P2` at its default 0 or at 2 they take the terminal's current background colour.
//!
//! The canvas grows as sixels extend it, in both directions, and is never larger than
//! [`MAX_IMAGE_SIDE`] per side and [`MAX_IMAGE_BYTES`] of RGBA8 data: a sixel that would push the
//! buffer past a cap paints nothing beyond it, later ones are ignored the same way, and `finish`
//! returns the image as far as it fits — an oversized sixel is truncated, not refused. All size
//! math is done in `u64` with saturating accumulators, so no input can overflow a length or make
//! an allocation larger than the caps. A stream that paints nothing and declares no size has zero
//! pixels and answers [`SixelError::Empty`].
//!
//! Remote input never panics, hangs or grows memory without bound here: bytes outside the commands
//! above are ignored, parameter values saturate instead of overflowing, an unterminated introducer
//! commits what it accumulated when the stream ends, a `!` repeat paints at most as many columns as
//! can fit, and every buffer is bounded by the caps above. Data arrives in arbitrary chunks (the
//! PTY splits sequences anywhere): numbers, colour introducers and raster attributes are parsed as
//! a state machine that continues across `put` calls, so a stream decodes identically whether it
//! arrived whole or one byte at a time.
//!
//! What is *not* here: where the image lands and what the text cursor does afterwards (the DECSDM
//! rules) belong to the placement side of Task 6; this module only turns bytes into pixels. The
//! `params` slice is positional as the DCS sends it — index 0 `P1`, index 1 `P2`, index 2 `P3`; a
//! missing value keeps its DEC default, and only `P2` changes decoding (`P1`'s aspect ratio and
//! `P3`'s grid size do not affect the pixels). Note that the vte parser flattens *omitted*
//! parameters away, so a caller that needs exact positions for a stream like `ESC P ;;1 q` must
//! re-insert them; ordinary streams (`q`, `0;1;q`, ...) are unaffected.

use super::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE};

/// The colour registers one image gets, private to it (the count XTSMGRAPHICS reports).
const REGISTERS: usize = 1_024;

/// A straight (not premultiplied) 8-bit RGBA colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel, 255 opaque.
    pub a: u8,
}

impl Rgba {
    /// An opaque colour from its channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// The pixel layout used in [`SixelImage::rgba`].
    fn bytes(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

/// One decoded image: `width * height * 4` bytes of straight RGBA8, row-major.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SixelImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// The pixels, four bytes per pixel (`r`, `g`, `b`, `a`), rows top to bottom.
    pub rgba: Vec<u8>,
}

/// Why a sixel stream produced no image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SixelError {
    /// Nothing was painted and no size was declared: the image would have zero pixels.
    Empty,
}

/// The VT340 factory-default colour map, indices 0 to 15, as documented by the VT340 Graphics
/// Programming manual (table 2-3) and measured on real hardware (the RGB values are DEC's 0-100
/// percent channels scaled to 0-255). The VT340 rotated its hue wheel so that 0° is blue; the
/// table stores explicit RGB, so that quirk does not reach the decoder's HLS conversion, which
/// follows the ordinary colour wheel with 0° at red — what sixel encoders produce and expect.
const VT340_PALETTE: [[u8; 3]; 16] = [
    [0, 0, 0],       // 0   black (background)
    [51, 51, 201],   // 1   blue
    [201, 33, 33],   // 2   red
    [51, 201, 51],   // 3   green
    [201, 51, 201],  // 4   magenta
    [51, 201, 201],  // 5   cyan
    [201, 201, 51],  // 6   yellow
    [117, 117, 117], // 7   gray 50% (foreground)
    [66, 66, 66],    // 8   gray 25%
    [84, 84, 150],   // 9   blue, low saturation
    [150, 66, 66],   // 10  red, low saturation
    [84, 150, 84],   // 11  green, low saturation
    [150, 84, 150],  // 12  magenta, low saturation
    [84, 150, 150],  // 13  cyan, low saturation
    [150, 150, 84],  // 14  yellow, low saturation
    [201, 201, 201], // 15  gray 75%
];

/// The starting colour registers: the VT340 palette for 0-15, everything else undefined until a
/// `#Pc;...` definition programs it (private to one image).
fn default_registers() -> [Option<Rgba>; REGISTERS] {
    let mut regs = [None; REGISTERS];
    for (i, &[r, g, b]) in VT340_PALETTE.iter().enumerate() {
        regs[i] = Some(Rgba::new(r, g, b));
    }
    regs
}

/// A decimal parameter list being read (`#`, `"`, or `!` introducer).
///
/// `vals` holds the parameters committed by a `;` so far; `cur`/`started` the one being typed.
/// Values saturate at [`u32::MAX`] instead of overflowing, and parameters past the introducer's
/// arity are read and dropped.
#[derive(Clone, Copy, Debug)]
struct Params {
    vals: [u32; 5],
    len: usize,
    cur: u32,
    started: bool,
}

impl Params {
    fn new() -> Self {
        Self { vals: [0; 5], len: 0, cur: 0, started: false }
    }

    /// Consume one byte as part of the list if it is a digit or a separator. Returns `false` for a
    /// byte that ends the list, which the caller then dispatches as a command.
    fn feed(&mut self, b: u8, arity: usize) -> bool {
        match b {
            b'0'..=b'9' => {
                self.cur = self.cur.saturating_mul(10).saturating_add((b - b'0') as u32);
                self.started = true;
                true
            },
            b';' => {
                if self.len < arity {
                    self.vals[self.len] = if self.started { self.cur } else { 0 };
                    self.len += 1;
                }
                self.cur = 0;
                self.started = false;
                true
            },
            _ => false,
        }
    }

    /// Commit the parameter being typed (at a terminator or the end of the stream) and return how
    /// many values there are. Parameters past the arity were already dropped.
    fn close(&mut self, arity: usize) -> &[u32] {
        if self.started && self.len < arity {
            self.vals[self.len] = self.cur;
            self.len += 1;
        }
        &self.vals[..self.len]
    }
}

/// Where inside an introducer the byte stream currently is.
#[derive(Clone, Copy, Debug)]
enum State {
    /// Between commands: a sixel character, `$`, `-` or a new introducer.
    Normal,
    /// After `#`: `[Pc; Pu; Px; Py; Pz]`, arity 5 (a lone `Pc` selects instead of defining).
    Color(Params),
    /// After `"`: `[Pan; Pad; Ph; Pv]`, arity 4.
    Raster(Params),
    /// After `!`: the repeat count, arity 1; the next byte is the character to repeat.
    Repeat(Params),
}

/// The sixel image being decoded from one DCS sequence.
#[derive(Clone, Debug)]
pub struct SixelDecoder {
    /// `P2=1`: pixels a zero bit or an unpainted region leaves alone stay transparent.
    transparent: bool,
    /// The terminal's current background colour, painted where the protocol says "background".
    default_bg: Rgba,
    /// This image's private colour registers; `None` is one not yet defined.
    registers: [Option<Rgba>; REGISTERS],
    /// The colour sixels paint with now; `None` while no defined register is selected.
    current: Option<Rgba>,
    /// The canvas, `width * height * 4` bytes, row-major, always exactly that long.
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    /// Sixel cursor: the column the next data character paints, and the top row of its band.
    x: u32,
    band: u32,
    state: State,
}

impl SixelDecoder {
    /// A decoder for one sixel sequence. `params` are the DCS parameters before the `q` in order;
    /// only index 1 (`P2`) matters, where a value of 1 asks for a transparent background.
    /// `default_bg` is the colour pixels get when the stream specifies "background" with an opaque
    /// (`P2` 0 or 2) image.
    pub fn new(params: &[u16], default_bg: Rgba) -> Self {
        Self {
            transparent: matches!(params.get(1).copied(), Some(1)),
            default_bg,
            registers: default_registers(),
            current: None,
            rgba: Vec::new(),
            width: 0,
            height: 0,
            x: 0,
            band: 0,
            state: State::Normal,
        }
    }

    /// Feed sixel data bytes, as many `put` calls and however the stream happens to split them.
    /// Bytes that are not part of a command this decoder know are ignored; nothing here panics or
    /// allocates past the image caps whatever arrives.
    pub fn put(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let state = std::mem::replace(&mut self.state, State::Normal);
            match state {
                State::Normal => self.dispatch(b),
                State::Color(mut p) => {
                    if p.feed(b, 5) {
                        self.state = State::Color(p);
                    } else {
                        let vals = p.close(5);
                        let (pc, rest) = vals.split_first().unwrap_or((&0, &[]));
                        self.commit_color(*pc, rest);
                        self.dispatch(b);
                    }
                },
                State::Raster(mut p) => {
                    if p.feed(b, 4) {
                        self.state = State::Raster(p);
                    } else {
                        let vals = p.close(4);
                        self.commit_raster(vals);
                        self.dispatch(b);
                    }
                },
                State::Repeat(mut p) => {
                    if p.feed(b, 1) {
                        self.state = State::Repeat(p);
                    } else {
                        // An omitted count repeats once (`!~` is a plain `~`). The character to
                        // repeat is a sixel data character; anything else ends the repeat without
                        // painting and acts as a command of its own.
                        let vals = p.close(1);
                        let count = if vals.is_empty() { 1 } else { vals[0] };
                        match b {
                            b'?'..=b'~' => self.paint_run(b - b'?', count),
                            _ => self.dispatch(b),
                        }
                    }
                },
            }
        }
    }

    /// The canvas as it stands mid-decode (`width`, `height`, buffer bytes): a test hook pinning
    /// that growth stops at the caps while data keeps arriving; `finish` is the real output.
    #[cfg(test)]
    pub(crate) fn canvas(&self) -> (u32, u32, usize) {
        (self.width, self.height, self.rgba.len())
    }

    /// Decode what arrived and hand out the image. A stream cut short before its terminator
    /// finishes the introducer it was in (a half-typed colour becomes that colour with the missing
    /// parameters at 0, as DEC specifies) and drops a repeat that never got its character. An
    /// image with no pixels is [`SixelError::Empty`].
    pub fn finish(mut self) -> Result<SixelImage, SixelError> {
        match std::mem::replace(&mut self.state, State::Normal) {
            State::Color(mut p) => {
                let vals = p.close(5);
                let (pc, rest) = vals.split_first().unwrap_or((&0, &[]));
                self.commit_color(*pc, rest);
            },
            State::Raster(mut p) => {
                let vals = p.close(4);
                self.commit_raster(vals);
            },
            State::Normal | State::Repeat(_) => {},
        }
        if self.width == 0 || self.height == 0 {
            return Err(SixelError::Empty);
        }
        Ok(SixelImage { width: self.width, height: self.height, rgba: self.rgba })
    }

    /// One byte between introducers.
    fn dispatch(&mut self, b: u8) {
        match b {
            b'?'..=b'~' => self.paint_run(b - b'?', 1),
            b'$' => self.x = 0,
            b'-' => {
                self.x = 0;
                self.band = self.band.saturating_add(6);
            },
            b'#' => self.state = State::Color(Params::new()),
            b'"' => self.state = State::Raster(Params::new()),
            b'!' => self.state = State::Repeat(Params::new()),
            // Digits and `;` outside an introducer, controls, everything else: no meaning in sixel
            // data, ignored.
            _ => {},
        }
    }

    /// Apply a colour introducer: `#Pc` selects register `Pc`, `#Pc;1;H;L;S` or `#Pc;2;R;G;B`
    /// defines it and selects the definition. A register past the last one this image has is
    /// ignored whole; an undefined register selected alone leaves nothing to paint with, so sixels
    /// are dropped until a defined one is selected.
    fn commit_color(&mut self, pc: u32, rest: &[u32]) {
        if rest.is_empty() {
            self.current = self.register(pc);
            return;
        }
        let mode = rest.first().copied().unwrap_or(0);
        let (x, y, z) = (
            rest.get(1).copied().unwrap_or(0),
            rest.get(2).copied().unwrap_or(0),
            rest.get(3).copied().unwrap_or(0),
        );
        let color = match mode {
            1 => hls_to_rgba(x, y, z),
            2 => rgb100_to_rgba(x, y, z),
            // An unknown coordinate system defines nothing and selects nothing.
            _ => return,
        };
        if let Some(slot) = self.registers.get_mut(pc as usize) {
            *slot = Some(color);
            self.current = Some(color);
        }
    }

    /// Apply a raster attribute command: `Ph` and `Pv` declare the image's pixel size, which the
    /// canvas grows to (the declared area counts as painted background). `Pan`/`Pad` describe how
    /// the terminal *displays* pixels and do not change these pixels.
    fn commit_raster(&mut self, vals: &[u32]) {
        let ph = vals.get(2).copied().unwrap_or(0);
        let pv = vals.get(3).copied().unwrap_or(0);
        self.ensure_canvas(ph, pv);
    }

    /// The colour of register `Pc`, or `None` when it is undefined or past the last register.
    fn register(&self, pc: u32) -> Option<Rgba> {
        self.registers.get(pc as usize).copied().flatten()
    }

    /// Paint `count` copies of the sixel column `value` (`value & 1 << row` is lit, row 0 on top)
    /// at the cursor, then move the cursor right past them — even past the caps, so a later `$` and
    /// repaint still lands correctly. A zero bit paints the background colour with an opaque image
    /// (`P2` 0 or 2) and leaves the pixel alone with `P2=1`, where an untouched pixel is
    /// transparent. Lit rows need a selected defined colour; without one they paint nothing.
    fn paint_run(&mut self, value: u8, count: u32) {
        if count == 0 {
            return;
        }
        let bg = if self.transparent { None } else { Some(self.default_bg) };
        let color = self.current;
        let mut rows: [Option<Rgba>; 6] = [None; 6];
        for (r, slot) in rows.iter_mut().enumerate() {
            *slot = if value >> r & 1 == 1 { color } else { bg };
        }
        // Nothing to write anywhere: every lit row has no colour selected, and every zero row is
        // left alone because the background is transparent (`P2=1`). Advance the cursor only.
        if rows.iter().all(Option::is_none) {
            self.x = self.x.saturating_add(count);
            return;
        }
        if self.band >= MAX_IMAGE_SIDE {
            // The band itself is below the last row a canvas can have: nothing of it can fit.
            self.x = self.x.saturating_add(count);
            return;
        }
        let cols = count.min(MAX_IMAGE_SIDE.saturating_sub(self.x));
        if cols > 0 {
            self.ensure_canvas(self.x + cols, self.band + 6);
        }
        for i in 0..cols {
            let x = self.x + i;
            if x >= self.width {
                // The byte cap trimmed the width: nothing from here rightwards fits.
                break;
            }
            for (r, &pixel) in rows.iter().enumerate() {
                let Some(pixel) = pixel else { continue };
                let y = self.band + r as u32;
                if y < self.height {
                    self.put_pixel(x, y, pixel);
                }
            }
        }
        self.x = self.x.saturating_add(count);
    }

    /// Write one pixel (row-major RGBA8). Callers have checked the bounds against `width`/`height`.
    fn put_pixel(&mut self, x: u32, y: u32, color: Rgba) {
        let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
        self.rgba[i..i + 4].copy_from_slice(&color.bytes());
    }

    /// Grow the canvas to cover `w_needed` by `h_needed`, capped at [`MAX_IMAGE_SIDE`] per side and
    /// [`MAX_IMAGE_BYTES`] of pixels, never shrinking. New area is background: the default
    /// background colour with an opaque image, transparent with `P2=1`. When even that cannot hold
    /// both dimensions, width keeps what the (new) height allows and height what the width allows —
    /// earlier pixels always survive, only new area is dropped. All math is overflow-safe: sizes
    /// are compared in `u64` against the caps before anything is reserved.
    fn ensure_canvas(&mut self, w_needed: u32, h_needed: u32) {
        let mut w = self.width.max(w_needed.min(MAX_IMAGE_SIDE));
        let mut h = self.height.max(h_needed.min(MAX_IMAGE_SIDE));
        loop {
            let area = (w as u64) * (h as u64) * 4;
            if area <= MAX_IMAGE_BYTES {
                break;
            }
            // Shrink the dimension that grew most recently, never below the current canvas (which
            // always fits by invariant, so each step makes progress and one of them stops).
            if h > self.height {
                let budget =
                    (MAX_IMAGE_BYTES / (w.max(1) as u64 * 4)).min(MAX_IMAGE_SIDE as u64) as u32;
                h = self.height.max(budget);
            } else {
                let budget =
                    (MAX_IMAGE_BYTES / (h.max(1) as u64 * 4)).min(MAX_IMAGE_SIDE as u64) as u32;
                w = self.width.max(budget);
            }
        }
        if w != self.width || h != self.height {
            self.resize_canvas(w, h);
        }
    }

    /// Rebuild the buffer at `w` by `h` (both at least the current size), keeping what overlaps and
    /// filling the rest with background.
    fn resize_canvas(&mut self, w: u32, h: u32) {
        let fill = if self.transparent { [0u8; 4] } else { self.default_bg.bytes() };
        let len = (w as usize) * (h as usize) * 4; // at most MAX_IMAGE_BYTES: checked by the caller
        let mut buf = vec![0u8; len];
        for chunk in buf.chunks_mut(4) {
            chunk.copy_from_slice(&fill);
        }
        let old_w = self.width as usize;
        let new_w = w as usize;
        for y in 0..self.height as usize {
            let src = y * old_w * 4;
            buf[new_w * y * 4..new_w * y * 4 + old_w * 4]
                .copy_from_slice(&self.rgba[src..src + old_w * 4]);
        }
        self.rgba = buf;
        self.width = w;
        self.height = h;
    }
}

/// `#Pc;1;H;L;S`: HLS to RGB. Hue is taken modulo 360 (0° at red, the ordinary colour wheel),
/// lightness and saturation clamp to 0-100. Channels round to nearest.
fn hls_to_rgba(hue: u32, light: u32, sat: u32) -> Rgba {
    let h = (hue % 360) as f32 / 360.0;
    let l = (light.min(100) as f32) / 100.0;
    let s = (sat.min(100) as f32) / 100.0;
    if s == 0.0 {
        let v = channel8(l);
        return Rgba::new(v, v, v);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    Rgba::new(
        channel8(hue_sector(p, q, h + 1.0 / 3.0)),
        channel8(hue_sector(p, q, h)),
        channel8(hue_sector(p, q, h - 1.0 / 3.0)),
    )
}

/// One channel of the HLS conversion: the hue wheel folded into `[0, 1]` and interpolated.
fn hue_sector(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    } else if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

/// A `[0, 1]` channel to 8 bits, rounding half away from zero.
fn channel8(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

/// `#Pc;2;R;G;B`: three 0-100 percent channels to 8 bits, rounding half away from zero; values
/// above 100 clamp.
fn rgb100_to_rgba(r: u32, g: u32, b: u32) -> Rgba {
    fn scale(v: u32) -> u8 {
        ((v.min(100) * 255 + 50) / 100) as u8
    }
    Rgba::new(scale(r), scale(g), scale(b))
}
