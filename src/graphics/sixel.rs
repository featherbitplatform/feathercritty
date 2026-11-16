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
//! command `"Pan;Pad;Ph;Pv`, whose `Ph`/`Pv` declare the image's minimum size in pixels; and the
//! colour introducer `#Pc` alone (select a register) or as `#Pc;Pu;Px;Py;Pz` defining one — `Pu=2`
//! gives RGB with each channel 0-100, `Pu=1` gives HLS with hue on the DEC wheel (0° is blue, 120°
//! red, 240° green) and lightness and saturation 0-100. A register definition also selects the
//! register. Every image gets its own private set of [`REGISTERS`] colour registers; 0 to 15 start
//! as the VT340 factory-default colour map (the Graphics Programming manual's table 2-3), the rest
//! start undefined.
//!
//! Pixels are square: the aspect parameters — `P1` of the DCS and `Pan`/`Pad` of the raster
//! attributes — say how a terminal *displays* the image and change no pixels here (a known limit;
//! where the image lands is placement, Task 6). Writing follows DEC semantics exactly: only a set
//! bit writes and a zero bit never changes a pixel, so colour passes overprint each other instead
//! of erasing. `P2` decides what the pixels that were *never* painted are: with `P2=1` they are
//! transparent (alpha 0); with `P2` at its default 0 or at 2 the canvas area is initialized to the
//! terminal's background colour as it is created or grown, and only 1 bits ever change it.
//!
//! The canvas tracks its logical painted extent and its allocated capacity separately. The capacity
//! grows geometrically — each side doubles until it covers what the next sixel needs — so a long
//! run costs a handful of reallocations instead of one per column, and `finish` crops it to the
//! logical size once. Capacity is never more than [`MAX_IMAGE_SIDE`] per side or
//! [`MAX_IMAGE_BYTES`] of RGBA8 data: a sixel that would push past a cap paints nothing beyond it,
//! later ones are ignored the same way, and `finish` returns the image as far as it fits — an
//! oversized sixel is truncated, not refused. When doubling would bust the byte cap but the
//! rectangle actually asked for still fits, the trim serves that rectangle instead: geometric
//! overshoot never truncates a legal image. Raster attributes allocate nothing up front: they only
//! raise a minimum logical size applied in `finish`, so a 15-byte header may declare 10 000×10 000
//! without the decoder reserving for it before the first sixel. All size math is done in `u64` with
//! saturating accumulators, so no input can overflow a length or make an allocation larger than the
//! caps. A stream that paints nothing and declares no size has zero pixels and answers
//! [`SixelError::Empty`].
//!
//! Remote input never panics, hangs or grows memory without bound here: bytes outside the commands
//! above are ignored, parameter values saturate instead of overflowing, an unterminated introducer
//! commits what it accumulated when the stream ends, a `!` repeat paints at most as many columns as
//! can fit, and every buffer is bounded by the caps above. Data arrives in arbitrary chunks (the
//! PTY splits sequences anywhere): numbers, colour introducers and raster attributes are parsed as
//! a state machine that continues across `put` calls, so a stream decodes identically whether it
//! arrived whole or one byte at a time.
//!
//! The `params` slice is positional as the DCS sends it — index 0 `P1`, index 1 `P2`, index 2 `P3`;
//! a missing value keeps its DEC default, and only `P2` changes decoding (`P1`'s aspect ratio and
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

/// The VT340 factory-default colour map, indices 0 to 15, exactly as table 2-3 of the VT340
/// Graphics Programming manual lists it: DEC's 0-100 percent channels, scaled to 8 bits by
/// [`percent_to_u8`] when the registers are built. The VT340 rotated its hue wheel so that HLS
/// hue 0° is blue; [`hls_to_rgba`] applies that rotation, and this table — being direct RGB
/// percentages — needs none.
const VT340_PALETTE_PCT: [[u32; 3]; 16] = [
    [0, 0, 0],    // 0   black (background)
    [20, 20, 80], // 1   blue
    [80, 13, 13], // 2   red
    [20, 80, 20], // 3   green
    [80, 20, 80], // 4   magenta
    [20, 80, 80], // 5   cyan
    [80, 80, 20], // 6   yellow
    [53, 53, 53], // 7   gray 50% (foreground)
    [26, 26, 26], // 8   gray 25%
    [33, 33, 60], // 9   blue, low saturation
    [60, 26, 26], // 10  red, low saturation
    [33, 60, 33], // 11  green, low saturation
    [60, 33, 60], // 12  magenta, low saturation
    [33, 60, 60], // 13  cyan, low saturation
    [60, 60, 33], // 14  yellow, low saturation
    [80, 80, 80], // 15  gray 75%
];

/// The starting colour registers: the VT340 palette for 0-15, everything else undefined until a
/// `#Pc;...` definition programs it (private to one image).
fn default_registers() -> [Option<Rgba>; REGISTERS] {
    let mut regs = [None; REGISTERS];
    for (i, &[r, g, b]) in VT340_PALETTE_PCT.iter().enumerate() {
        regs[i] = Some(Rgba::new(percent_to_u8(r), percent_to_u8(g), percent_to_u8(b)));
    }
    regs
}

/// A DEC 0-100 percent channel to 8 bits, rounding half away from zero; values above 100 clamp.
fn percent_to_u8(v: u32) -> u8 {
    ((v.min(100) * 255 + 50) / 100) as u8
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
    /// `P2=1`: pixels that were never painted stay transparent; otherwise canvas area takes the
    /// background colour as it is created.
    transparent: bool,
    /// The terminal's current background colour, what never-painted pixels of an opaque image are.
    default_bg: Rgba,
    /// This image's private colour registers; `None` is one not yet defined.
    registers: [Option<Rgba>; REGISTERS],
    /// The colour sixels paint with now; `None` while no defined register is selected.
    current: Option<Rgba>,
    /// The working buffer, [`cap_w`](Self::cap_w) pixels wide per row times
    /// [`cap_h`](Self::cap_h) rows — strided, possibly wider and taller than anything painted, and
    /// every never-painted pixel of it already holds the fill `P2` asks for.
    buf: Vec<u8>,
    cap_w: u32,
    cap_h: u32,
    /// The extent sixel data has actually claimed (a lit column claims its whole band): what
    /// [`finish`](Self::finish) crops the buffer to, unless a raster attribute declares more.
    painted_w: u32,
    painted_h: u32,
    /// The size the last `"Pan;Pad;Ph;Pv` declared (capped), a minimum logical size applied in
    /// [`finish`](Self::finish); it never allocates anything by itself.
    declared_w: u32,
    declared_h: u32,
    /// Sixel cursor: the column the next data character paints, and the top row of its band.
    x: u32,
    band: u32,
    state: State,
}

impl SixelDecoder {
    /// A decoder for one sixel sequence. `params` are the DCS parameters before the `q` in order;
    /// only index 1 (`P2`) matters, where a value of 1 asks for a transparent background.
    /// `default_bg` is what never-painted pixels of an opaque (`P2` 0 or 2) image are.
    pub fn new(params: &[u16], default_bg: Rgba) -> Self {
        Self {
            transparent: matches!(params.get(1).copied(), Some(1)),
            default_bg,
            registers: default_registers(),
            current: None,
            buf: Vec::new(),
            cap_w: 0,
            cap_h: 0,
            painted_w: 0,
            painted_h: 0,
            declared_w: 0,
            declared_h: 0,
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

    /// The decode as it stands mid-stream (`painted_w`, `painted_h`, allocated bytes): a test hook
    /// pinning that capacity stays capped while data keeps arriving; `finish` is the real output.
    #[cfg(test)]
    pub(crate) fn canvas(&self) -> (u32, u32, usize) {
        (self.painted_w, self.painted_h, self.buf.len())
    }

    /// Decode what arrived and hand out the image. A stream cut short before its terminator
    /// finishes the introducer it was in (a half-typed colour becomes that colour with the missing
    /// parameters at 0, as DEC specifies) and drops a repeat that never got its character. The
    /// image is the painted extent, or the declared size if one is larger, cropped out of the
    /// strided buffer; an image with no pixels is [`SixelError::Empty`].
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
        let mut width = self.painted_w.max(self.declared_w);
        let mut height = self.painted_h.max(self.declared_h);
        if width == 0 || height == 0 {
            return Err(SixelError::Empty);
        }
        // The byte cap can only bind on declared area — painted area fits by invariant. Trim
        // deterministically: height to what the width allows, then width to what that height
        // allows, each stopping at the painted extent (which always fits, so every round makes
        // progress and both floors together end the loop with room).
        loop {
            if (width as u64) * (height as u64) * 4 <= MAX_IMAGE_BYTES {
                break;
            }
            let budget = (MAX_IMAGE_BYTES / (width.max(1) as u64 * 4)).min(MAX_IMAGE_SIDE as u64);
            height = height.min(budget as u32).max(self.painted_h);
            if (width as u64) * (height as u64) * 4 <= MAX_IMAGE_BYTES {
                break;
            }
            let budget = (MAX_IMAGE_BYTES / (height.max(1) as u64 * 4)).min(MAX_IMAGE_SIDE as u64);
            width = width.min(budget as u32).max(self.painted_w);
        }
        // Crop once: never-painted pixels — inside the capacity or beyond it, where nothing was
        // ever allocated — are the fill `P2` asks for.
        let fill = if self.transparent { [0u8; 4] } else { self.default_bg.bytes() };
        let (w, h) = (width as usize, height as usize);
        let mut rgba = pattern_vec(fill, w * h * 4);
        let copy_cols = w.min(self.cap_w as usize) * 4;
        for y in 0..h.min(self.cap_h as usize) {
            let src = y * self.cap_w as usize * 4;
            let dst = y * w * 4;
            rgba[dst..dst + copy_cols].copy_from_slice(&self.buf[src..src + copy_cols]);
        }
        Ok(SixelImage { width, height, rgba })
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

    /// Apply a raster attribute command: `Ph` and `Pv` declare a minimum pixel size for the image,
    /// enforced in [`finish`](Self::finish). Nothing is allocated here — a header alone must not
    /// reserve for the size it declares. `Pan`/`Pad` describe how the terminal *displays* pixels
    /// and change no pixels here.
    fn commit_raster(&mut self, vals: &[u32]) {
        let ph = vals.get(2).copied().unwrap_or(0);
        let pv = vals.get(3).copied().unwrap_or(0);
        self.declared_w = self.declared_w.max(ph.min(MAX_IMAGE_SIDE));
        self.declared_h = self.declared_h.max(pv.min(MAX_IMAGE_SIDE));
    }

    /// The colour of register `Pc`, or `None` when it is undefined or past the last register.
    fn register(&self, pc: u32) -> Option<Rgba> {
        self.registers.get(pc as usize).copied().flatten()
    }

    /// Paint `count` copies of the sixel column `value` (`value & 1 << row` is lit, row 0 on top)
    /// at the cursor, then move the cursor right past them — even past the caps, so a later `$` and
    /// repaint still lands correctly. Only lit rows are written, and only with a selected defined
    /// colour: a zero bit never writes, and without a colour a lit row paints nothing, so those
    /// columns just move the cursor. A column that does paint claims its whole band — the rows in
    /// it that no bit lit are background (or transparent) by `P2`, not painted here.
    fn paint_run(&mut self, value: u8, count: u32) {
        if count == 0 || value == 0 {
            self.x = self.x.saturating_add(count);
            return;
        }
        let Some(color) = self.current else {
            self.x = self.x.saturating_add(count);
            return;
        };
        if self.band >= MAX_IMAGE_SIDE {
            // The band itself is below the last row a canvas can have: nothing of it can fit.
            self.x = self.x.saturating_add(count);
            return;
        }
        let cols = count.min(MAX_IMAGE_SIDE.saturating_sub(self.x));
        if cols > 0 {
            self.ensure_capacity(self.x + cols, self.band + 6);
            for i in 0..cols {
                let x = self.x + i;
                if x >= self.cap_w {
                    // The byte cap trimmed the width: nothing from here rightwards fits.
                    break;
                }
                for r in 0..6u32 {
                    if value >> r & 1 == 0 {
                        continue;
                    }
                    let y = self.band + r;
                    if y < self.cap_h {
                        self.put_pixel(x, y, color);
                    }
                }
            }
            self.painted_w = self.painted_w.max((self.x + cols).min(self.cap_w));
            self.painted_h = self.painted_h.max((self.band + 6).min(self.cap_h));
        }
        self.x = self.x.saturating_add(count);
    }

    /// Write one pixel (row-major RGBA8 into the strided buffer). Callers have checked the bounds
    /// against `cap_w`/`cap_h`.
    fn put_pixel(&mut self, x: u32, y: u32, color: Rgba) {
        let i = ((y as usize) * (self.cap_w as usize) + x as usize) * 4;
        self.buf[i..i + 4].copy_from_slice(&color.bytes());
    }

    /// Grow the allocated capacity to cover `w_needed` by `h_needed`. Each side doubles until it
    /// covers the need (so a run of columns costs logarithmically many reallocations, each copying
    /// the old rows once), clamped to [`MAX_IMAGE_SIDE`] per side and to [`MAX_IMAGE_BYTES`] in
    /// total. When the doubled pair busts the byte cap, the trim serves the rectangle actually
    /// asked for — the painted extent plus what this sixel claims — if that still fits (capacity
    /// may shrink toward it, since never-painted area is only fill): geometric overshoot never
    /// denies a legal image its area. When even the asked-for rectangle busts the cap, only the
    /// current capacity is a floor: no painted pixel is ever lost, only newly asked-for area is
    /// dropped, width keeping what the height allows and height what the width allows. All math is
    /// overflow-safe: sizes are compared in `u64` against the caps before anything is reserved.
    fn ensure_capacity(&mut self, w_needed: u32, h_needed: u32) {
        let target_w = w_needed.min(MAX_IMAGE_SIDE);
        let target_h = h_needed.min(MAX_IMAGE_SIDE);
        if target_w <= self.cap_w && target_h <= self.cap_h {
            return;
        }
        let mut w = doubled_capacity(self.cap_w, target_w);
        let mut h = doubled_capacity(self.cap_h, target_h);
        let logical_w = self.painted_w.max(target_w);
        let logical_h = self.painted_h.max(target_h);
        let (floor_w, floor_h) = if (logical_w as u64) * (logical_h as u64) * 4 <= MAX_IMAGE_BYTES {
            (logical_w, logical_h)
        } else {
            (self.cap_w, self.cap_h)
        };
        loop {
            if (w as u64) * (h as u64) * 4 <= MAX_IMAGE_BYTES {
                break;
            }
            // Trim height to what the width allows first, then width to what that height allows,
            // each floored at `floor_*` (a pair that fits by the branch above): a round can only
            // leave both untouched when both hit their floors, and then the area fits too.
            let budget = (MAX_IMAGE_BYTES / (w.max(1) as u64 * 4)).min(MAX_IMAGE_SIDE as u64);
            h = h.min(budget as u32).max(floor_h);
            if (w as u64) * (h as u64) * 4 <= MAX_IMAGE_BYTES {
                break;
            }
            let budget = (MAX_IMAGE_BYTES / (h.max(1) as u64 * 4)).min(MAX_IMAGE_SIDE as u64);
            w = w.min(budget as u32).max(floor_w);
        }
        self.resize_capacity(w, h);
    }

    /// Rebuild the buffer at `cap_w` by `cap_h` (each side at least [`painted_w`](Self::painted_w)
    /// / [`painted_h`](Self::painted_h), so no painted pixel is lost even when a side shrinks),
    /// keeping what overlaps and filling everything else with the fill `P2` asks for:
    /// background-coloured area that is later only ever changed by 1 bits, or transparent area.
    fn resize_capacity(&mut self, w: u32, h: u32) {
        if w == self.cap_w && h == self.cap_h {
            return;
        }
        let fill = if self.transparent { [0u8; 4] } else { self.default_bg.bytes() };
        // At most MAX_IMAGE_BYTES: the caller's cap loop checked it before anything was reserved.
        let mut buf = pattern_vec(fill, (w as usize) * (h as usize) * 4);
        let (old_w, new_w) = (self.cap_w as usize * 4, w as usize * 4);
        let copy_w = old_w.min(new_w);
        for y in 0..(self.cap_h.min(h) as usize) {
            let src = y * old_w;
            let dst = y * new_w;
            buf[dst..dst + copy_w].copy_from_slice(&self.buf[src..src + copy_w]);
        }
        self.buf = buf;
        self.cap_w = w;
        self.cap_h = h;
    }
}

/// The capacity for one side: the current capacity doubled until it covers `needed`, capped at
/// [`MAX_IMAGE_SIDE`] (a side that needs nothing gets its current value back, or 1 when there is
/// no capacity yet).
fn doubled_capacity(current: u32, needed: u32) -> u32 {
    let mut c = current.max(1);
    while c < needed.min(MAX_IMAGE_SIDE) && c < MAX_IMAGE_SIDE {
        c = (c * 2).min(MAX_IMAGE_SIDE);
    }
    c.max(current)
}

/// `len` bytes of a repeating four-byte pixel pattern: zeroed pages when the pattern is all zero
/// (the transparent fill), otherwise the pattern doubled into place, so filling megabytes costs
/// logarithmically many memcpys instead of one store per byte.
fn pattern_vec(pattern: [u8; 4], len: usize) -> Vec<u8> {
    if pattern == [0u8; 4] {
        return vec![0u8; len];
    }
    if len == 0 {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&pattern);
    while buf.len() * 2 <= len {
        let n = buf.len();
        buf.extend_from_within(..n);
    }
    let tail = len - buf.len();
    buf.extend_from_within(..tail);
    buf
}

/// `#Pc;1;H;L;S`: HLS to RGB. The hue is a DEC VT340 wheel position — 0° is blue, 120° red, 240°
/// green — so it is rotated by 240° onto the standard wheel (where 0° is red) before converting;
/// lightness and saturation clamp to 0-100. Channels round to nearest.
fn hls_to_rgba(hue: u32, light: u32, sat: u32) -> Rgba {
    let h = (((hue % 360) + 240) % 360) as f32 / 360.0;
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

/// `#Pc;2;R;G;B`: three 0-100 percent channels to 8 bits ([`percent_to_u8`], the same scaling the
/// factory palette uses).
fn rgb100_to_rgba(r: u32, g: u32, b: u32) -> Rgba {
    Rgba::new(percent_to_u8(r), percent_to_u8(g), percent_to_u8(b))
}
