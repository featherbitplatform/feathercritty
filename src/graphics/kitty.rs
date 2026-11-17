// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! The kitty graphics protocol state machine (feathercritty).
//!
//! [`KittyParser`] is fed by the APC callbacks of [`crate::vte`]: `apc_start` calls
//! [`start`](KittyParser::start), each `apc_put` a [`put`](KittyParser::put), `apc_end` an
//! [`end`](KittyParser::end) (which may return a [`KittyCommand`]) and `apc_abort` an
//! [`abort`](KittyParser::abort). A sequence is `ESC _ G <control data> ; <base64 payload> ESC \`
//! where the control data is a comma-separated list of `key=value` pairs. APC content that does
//! not start with `G` is not a graphics sequence and is ignored without a reply.
//!
//! What this parser supports: direct and chunked transmission (`t=d`, `m=1`/`m=0`) of raw RGB
//! and RGBA data (`f=24`/`f=32`) and PNG (`f=100`, sized from its header), optionally zlib
//! compressed (`o=z`); placements (`a=T`, `a=p`) with cell size, source rectangle, cell pixel
//! offsets, z-index, virtual placements for Unicode placeholders (`U=1`) and the cursor
//! movement policy (`C`); deletion (`a=d` with its selectors); and the support query (`a=q`).
//! Chunked payloads accumulate across APC sequences keyed by `i`/`I`: a continuation frame may
//! carry only `m`, `q` and optionally the `i` or `I` naming the transmission it belongs to; a
//! frame carrying any other key starts a new transmission, silently abandoning an unfinished
//! one (the protocol requires clients to finish chunks before sending anything else).
//!
//! What it refuses, and with which code: file-based media `t=f`, `t=t` and `t=s` with `EBADF`
//! (this fork never reads local files); indexed color formats `f=8`/`f=16` and the animation
//! actions `a=f`, `a=a`, `a=c` with `ENOTSUPPORTED`; anything malformed — unknown or
//! unsupported keys (`S`, `O`, `N`, `e`, `P`, `Q`, `H`, `V`, `F` among them), bad value types,
//! both `i` and `I` in one frame, an out-of-range integer, a truncated compressed stream, raw
//! data that does not match its declared size (compressed data longer than declared is refused
//! without inflating the excess) — with `EINVAL`; images over the size limits of
//! [`crate::graphics::limits`] and transmissions past [`MAX_PAYLOAD_BYTES`] with `ENOMEM`; PNG
//! data whose header is missing or unreadable with `EBADPNG`. A transmission that carries no
//! payload decodes to nothing and is silently ignored: there is no image to store or place. The
//! query action `a=q` is checked exactly as the transmission it asks about, so a client probing
//! which transfer media work gets `EBADF` for the ones this terminal will not accept instead of
//! an `OK` that every later image would contradict.
//!
//! Rejections do not travel as commands: they sit in a one-slot side channel,
//! [`take_reject`](KittyParser::take_reject), until the caller answers them with
//! [`reply`], which echoes the id and — for an image addressed by number — the number too. Bad
//! control data never disturbs a chunked upload in progress — only a payload failure, a delete
//! command or a frame that starts another transmission ends one (the protocol asks a delete to
//! abort an upload). Time is injected: `end(now)` and [`tick`](KittyParser::tick) drop a
//! transmission that has made no progress for [`STALL`] with `ENOMEM` — `tick` so that an idle
//! terminal releases a client that stopped mid-upload, counting an open APC frame as progress only
//! while bytes keep arriving into it — and tests never sleep. Every buffer is
//! bounded: the staged payload of one APC frame and the accumulated bytes of one transmission by
//! [`MAX_PAYLOAD_BYTES`], the inflated output by [`MAX_IMAGE_BYTES`] or, for a declared raw
//! format, by its own declared size; base64 is decoded incrementally by [`Decoder`], which carries
//! at most three characters between chunks and treats padding that ends one chunk as the end of the
//! payload. A dropped transmission releases what it held.

use std::time::Instant;

use flate2::{Decompress, FlushDecompress, Status};

use super::b64::{B64Error, Decoder};
use super::limits::{MAX_IMAGE_BYTES, MAX_IMAGE_SIDE, MAX_PAYLOAD_BYTES, STALL};

/// The PNG file magic, the sniffed marker for `f=100` data.
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The longest one `key=value` pair that can be buffered while parsing control data. Every
/// supported value is at most eleven ASCII bytes, so this only bounds hostile input.
const PAIR_CAP: usize = 64;

/// The output buffer of the incremental inflate, in bytes.
const INFLATE_SPARE: usize = 1 << 16;

/// A result code the terminal reports to the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyError {
    /// A file-based transmission medium was used (files are never read here).
    EBADF,
    /// Malformed control data or payload.
    EINVAL,
    /// No image with the given id.
    ENOENT,
    /// An image or transmission over the size limits.
    ENOMEM,
    /// PNG data whose header is missing or unreadable.
    EBADPNG,
    /// A protocol feature this terminal does not support.
    ENOTSUPPORTED,
}

impl KittyError {
    fn ecode(self) -> &'static str {
        match self {
            KittyError::EBADF => "EBADF",
            KittyError::EINVAL => "EINVAL",
            KittyError::ENOENT => "ENOENT",
            KittyError::ENOMEM => "ENOMEM",
            KittyError::EBADPNG => "EBADPNG",
            KittyError::ENOTSUPPORTED => "ENOTSUPPORTED",
        }
    }

    fn message(self) -> &'static str {
        match self {
            KittyError::EBADF => "cannot read image file",
            KittyError::EINVAL => "invalid control data",
            KittyError::ENOENT => "image not found",
            KittyError::ENOMEM => "too much image data",
            KittyError::EBADPNG => "malformed PNG data",
            KittyError::ENOTSUPPORTED => "not supported by this terminal",
        }
    }
}

/// A sequence the parser refused, with what [`reply`] needs to answer it: the id parsed before
/// the failure (0 when none was given) and the quiet level that suppresses the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyReject {
    /// The `i` of the sequence that failed, 0 when it carried none.
    pub id: u32,
    /// The `I` image number of the sequence that failed, for [`reply`] to echo.
    pub number: Option<u32>,
    /// The failure to report.
    pub error: KittyError,
    /// The sequence's quiet level, for `reply`.
    pub quiet: u8,
}

/// Builds a graphics protocol response frame: `ESC _ G i=<id>[,I=<number>][,p=<pid>]
/// ; OK|<ECODE>:<message> ESC \`. When the client addressed an image by number the terminal
/// answers with the id it assigned *and* echoes the number (`i=99,I=13` for `I=13`). A placement
/// id only travels with an image id (the protocol ignores placement ids on images without one),
/// and nothing is echoed for an unnamed, unnumbered image. `q=1` suppresses the OK answer, `q=2`
/// suppresses every answer; failures survive `q=1`.
pub fn reply(
    id: u32,
    number: Option<u32>,
    placement: Option<u32>,
    result: Result<(), KittyError>,
    quiet: u8,
) -> Option<Vec<u8>> {
    let ok = result.is_ok();
    if quiet >= 2 || (quiet == 1 && ok) {
        return None;
    }
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(b"\x1b_G");
    if id != 0 {
        out.extend_from_slice(format!("i={id}").as_bytes());
    }
    if let Some(n) = number {
        // A number alone (no id yet, e.g. a rejection before an id was assigned) still travels.
        let sep = if id == 0 { "" } else { "," };
        out.extend_from_slice(format!("{sep}I={n}").as_bytes());
    }
    if id != 0 {
        if let Some(pid) = placement.filter(|p| *p != 0) {
            out.extend_from_slice(format!(",p={pid}").as_bytes());
        }
    }
    out.push(b';');
    match result {
        Ok(()) => out.extend_from_slice(b"OK"),
        Err(error) => {
            out.extend_from_slice(error.ecode().as_bytes());
            out.push(b':');
            out.extend_from_slice(error.message().as_bytes());
        },
    }
    out.extend_from_slice(b"\x1b\\");
    Some(out)
}

/// The pixel format of transmitted image data. For PNG the size comes from the header; for raw
/// formats it is the `s`/`v` the sender declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyFormat {
    /// PNG data; the embedder decodes it. Size read from the IHDR.
    Png { width: u32, height: u32 },
    /// 3 bytes per pixel, sRGB, no alpha.
    Rgb { width: u32, height: u32 },
    /// 4 bytes per pixel, straight (non-premultiplied) alpha.
    Rgba { width: u32, height: u32 },
}

/// A source rectangle in pixels, the `x,y,w,h` of a placement. A zero `w` or `h` means "to the
/// end of the image" (the protocol defaults), and the displayed area is the intersection of the
/// rectangle with the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// An image placement: where and how an image is displayed. `cols`/`rows` of 0 mean "compute
/// from the image size and aspect". `virtual_` (`U=1`) creates no pixels: cells with the
/// placeholder character refer to the image instead. `cursor_movement` is false only for `C=1`
/// (do not move the cursor when placing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyPlace {
    /// The `i` of the image to place (`None` with no id).
    pub id: Option<u32>,
    /// The `I` image number to place, acted on as "the newest image with this number".
    pub number: Option<u32>,
    /// The `p` placement id (`None` for an unnamed placement).
    pub placement_id: Option<u32>,
    pub cols: u32,
    pub rows: u32,
    pub src: Option<Rect>,
    pub x_off: u32,
    pub y_off: u32,
    pub z: i32,
    pub virtual_: bool,
    pub cursor_movement: bool,
    /// The frame's quiet level, for `reply`.
    pub quiet: u8,
}

/// What a delete command selects. The names follow the protocol's `d=` values; `x`/`y` in cell
/// selectors are 1-based cell coordinates as the protocol defines them, and in `IdRange` they
/// are the inclusive id bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteTarget {
    /// `d=a` / `d=A`: every placement on screen.
    All,
    /// `d=i` / `d=I`: one image (optionally one placement of it).
    ImageId { image: Option<u32>, placement: Option<u32> },
    /// `d=n` / `d=N`: the newest image with this number.
    Number { number: Option<u32>, placement: Option<u32> },
    /// `d=c` / `d=C`: placements under the cursor.
    CursorCells,
    /// `d=p` / `d=P`: placements intersecting a cell.
    Cell { x: u32, y: u32 },
    /// `d=q` / `d=Q`: placements intersecting a cell at a z-index.
    CellZ { x: u32, y: u32, z: i32 },
    /// `d=x` / `d=X`: placements intersecting a column.
    Column(u32),
    /// `d=y` / `d=Y`: placements intersecting a row.
    Row(u32),
    /// `d=z` / `d=Z`: placements at a z-index.
    ZIndex(i32),
    /// `d=r` / `d=R`: images with an id in a range.
    IdRange { from: u32, to: u32 },
}

/// A delete command (`a=d`). `free_data` marks the uppercase selectors, which release the
/// stored image as well as its placements. Any delete aborts a chunked upload in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyDelete {
    pub target: DeleteTarget,
    pub free_data: bool,
    /// The frame's quiet level, for `reply`.
    pub quiet: u8,
}

/// A completed graphics sequence. Rejections are not commands; see [`KittyReject`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KittyCommand {
    /// Image data arrived (`a=t`, `a=T` and their chunked forms). `then_place` is the placement
    /// carried by an `a=T` frame. A reply for a transmitted image with an id depends on the
    /// embedder's decode result, so the caller answers it from `Term::image_decoded`.
    Transmit {
        id: Option<u32>,
        number: Option<u32>,
        format: KittyFormat,
        bytes: Vec<u8>,
        then_place: Option<KittyPlace>,
        quiet: u8,
    },
    /// Display a stored image (`a=p`).
    Place(KittyPlace),
    /// Delete placements and possibly data (`a=d`).
    Delete(KittyDelete),
    /// The support query (`a=q`): answer `OK` when images are enabled, `ENOTSUPPORTED` when not.
    Query { id: u32, quiet: u8 },
}

/// Where inside the current APC frame's content the parser is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    /// Waiting for the leading `G`; anything else makes the whole sequence a foreign APC.
    WaitingG,
    /// Inside the comma-separated control data.
    Control,
    /// After the first `;`: staging base64 payload text.
    Payload,
    /// Done with this frame: consumed and discarded (foreign, rejected or over cap).
    Skip,
}

/// The control keys of one APC frame, with the protocol's value types. A key that was not sent
/// is `None`; a key sent twice keeps its last value.
#[derive(Clone, Copy, Debug, Default)]
struct Keys {
    a: Option<u8>,
    t: Option<u8>,
    f: Option<u32>,
    o: Option<u8>,
    m: Option<bool>,
    i: Option<u32>,
    /// `I`: the image number.
    number: Option<u32>,
    p: Option<u32>,
    q: Option<u8>,
    s: Option<u32>,
    v: Option<u32>,
    x: Option<u32>,
    y: Option<u32>,
    w: Option<u32>,
    h: Option<u32>,
    /// `X`/`Y`: offsets into a cell, in pixels.
    x_off: Option<u32>,
    y_off: Option<u32>,
    c: Option<u32>,
    r: Option<u32>,
    /// `C`: 1 means "do not move the cursor when placing".
    no_move: Option<u32>,
    z: Option<i32>,
    /// `U`: 1 means a virtual placement (Unicode placeholders).
    virtual_: Option<u32>,
    d: Option<u8>,
}

/// The state of the APC frame being parsed.
#[derive(Debug)]
struct Frame {
    /// `start` ran and `end`/`abort` has not since.
    active: bool,
    region: Region,
    /// Text of the control pair being read (bounded by [`PAIR_CAP`]).
    pair: Vec<u8>,
    keys: Keys,
    /// One bit per key seen, for the continuation-frame test.
    used: u64,
    /// Staged base64 payload text of this frame (bounded by [`MAX_PAYLOAD_BYTES`]).
    text: Vec<u8>,
    /// The reason this frame will be rejected at `end`, if it will.
    error: Option<KittyError>,
}

impl Frame {
    fn inactive() -> Frame {
        Frame {
            active: false,
            region: Region::Skip,
            pair: Vec::new(),
            keys: Keys::default(),
            used: 0,
            text: Vec::new(),
            error: None,
        }
    }
}

/// A chunked transmission in progress, keyed by the `i`/`I` its first frame declared.
struct Pending {
    id: Option<u32>,
    number: Option<u32>,
    quiet: u8,
    /// The declared format (`f`), or `None` when it is to be sniffed from the payload.
    fmt_spec: Option<u32>,
    /// The declared raw dimensions (`s`/`v`) as sent.
    width_hint: Option<u32>,
    height_hint: Option<u32>,
    compressed: bool,
    /// Accumulated payload: decoded bytes, or inflated bytes when compressed.
    out: Vec<u8>,
    inflator: Option<Decompress>,
    /// Compressed bytes fed to the inflator, for the input cap.
    fed: u64,
    stream_end: bool,
    /// The placement carried by an `a=T` frame, if any.
    place: Option<KittyPlace>,
    /// Bytes the declared raw format (`f=24|32`) expects: `s * v * bpp`. For `o=z` data the
    /// inflate stops here — compressed pixel data longer than its declaration is EINVAL rather
    /// than a full inflate of whatever the client sent.
    raw_limit: Option<usize>,
    last_progress: Instant,
}

impl Pending {
    /// The byte count a raw format declared, as the inflate's stop line: compressed pixel data
    /// longer than its declaration is EINVAL. `None` (PNG or an undeclared format) leaves the
    /// image cap as the only ceiling.
    fn declared_size(&self) -> Option<u64> {
        self.raw_limit.map(|n| n as u64)
    }
}

/// The kitty graphics protocol state machine; see the [module docs](self).
pub struct KittyParser {
    frame: Frame,
    pending: Option<Pending>,
    /// The payload base64, decoded incrementally across chunks (at most three characters carried,
    /// and padding that ends one chunk ends the payload).
    b64: Decoder,
    reject: Option<KittyReject>,
    /// Bumps whenever bytes arrive in an open frame, so `tick` can tell a frame that is still
    /// receiving from one that has gone quiet ([`tick`](Self::tick)).
    put_gen: u64,
    /// The last `put_gen` value `tick` observed; equal to [`put_gen`](Self::put_gen) means the
    /// open frame made no progress since the previous check.
    seen_put_gen: u64,
}

impl Default for KittyParser {
    fn default() -> Self {
        Self {
            frame: Frame::inactive(),
            pending: None,
            b64: Decoder::default(),
            reject: None,
            put_gen: 0,
            seen_put_gen: 0,
        }
    }
}

impl KittyParser {
    /// A parser with no transmission in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// An APC sequence began (`apc_start`). A frame left unterminated (a caller that starts
    /// without ending) is discarded.
    pub fn start(&mut self) {
        if self.frame.active {
            // A frame started without ending (the vte parser aborted it): drop it, then set up
            // the new one. An upload completed by earlier chunks keeps its data.
            self.abort();
        }
        self.frame = Frame::inactive();
        self.frame.active = true;
        self.frame.region = Region::WaitingG;
        // A new frame starts its own progress accounting for `tick`.
        self.seen_put_gen = self.put_gen;
    }

    /// APC content arrived (`apc_put`), in arbitrary slices.
    pub fn put(&mut self, bytes: &[u8]) {
        if !self.frame.active {
            return;
        }
        if !bytes.is_empty() {
            self.put_gen += 1;
        }
        match self.frame.region {
            Region::WaitingG => {
                if let Some((&first, rest)) = bytes.split_first() {
                    if first == b'G' {
                        self.frame.region = Region::Control;
                        self.feed_control(rest);
                    } else {
                        self.frame.region = Region::Skip;
                    }
                }
            },
            Region::Control => self.feed_control(bytes),
            Region::Payload => self.stage_payload(bytes),
            Region::Skip => {},
        }
    }

    /// Stage payload text for this frame, bounded by the transmission cap.
    fn stage_payload(&mut self, bytes: &[u8]) {
        if self.frame.text.len() + bytes.len() > MAX_PAYLOAD_BYTES {
            // One APC frame carrying more than a whole transmission's cap.
            self.reject_frame(KittyError::ENOMEM);
        } else {
            self.frame.text.extend_from_slice(bytes);
        }
    }

    /// The APC ended (`apc_end`). Returns the command the sequence completed, if any; rejections
    /// wait in [`take_reject`](Self::take_reject). `now` drives the stall timeout: a chunked
    /// transmission that received no chunk since `now - STALL` is dropped with `ENOMEM`, unless
    /// this very call touched it.
    pub fn end(&mut self, now: Instant) -> Option<KittyCommand> {
        // Control text that never saw its closing `;` still holds one pair to process: the last
        // key of a frame is as binding as the first.
        if self.frame.active
            && self.frame.error.is_none()
            && self.frame.region == Region::Control
            && !self.frame.pair.is_empty()
        {
            self.process_pair();
        }
        let frame = std::mem::replace(&mut self.frame, Frame::inactive());
        let mut touched = false;
        let command = if !frame.active {
            None // `end` without `start`: nothing was parsed, but the stall check below runs.
        } else if let Some(error) = frame.error {
            self.reject = self.reject.take().or(Some(KittyReject {
                id: frame.keys.i.unwrap_or(0),
                number: frame.keys.number,
                error,
                quiet: frame.keys.q.unwrap_or(0),
            }));
            None
        } else {
            match frame.region {
                Region::WaitingG | Region::Skip => None, // a foreign or empty APC
                Region::Control | Region::Payload => self.dispatch(frame, now, &mut touched),
            }
        };
        if !touched {
            self.drop_stalled(now);
        }
        command
    }

    /// The APC was cancelled by `CAN`/`SUB` (`apc_abort`). The frame in progress is discarded;
    /// a transmission keeps what earlier completed chunks stored. A compressed stream cannot be
    /// rewound mid-chunk, so its aborted chunk simply contributes nothing (the inflate state
    /// never saw this frame's bytes: they are fed only when a frame ends).
    pub fn abort(&mut self) {
        self.frame = Frame::inactive();
    }

    /// Nothing arrived on the wire: callers run this on a timer so that a client which stops
    /// mid-upload while the terminal sits idle is released anyway. It drops a chunked transmission
    /// that has made no progress for [`STALL`], with the `ENOMEM` rejection waiting in
    /// [`take_reject`](Self::take_reject). An open APC frame does not exempt the transmission:
    /// the frame counts as progress only while bytes keep arriving into it — if it has received
    /// nothing since the previous check and the upload's own clock passed [`STALL`], the stalled
    /// upload is dropped under it (the frame's staged payload belongs to a transmission that no
    /// longer exists, and its `end` treats it as starting fresh). A client streaming one huge slow
    /// chunk is never dropped: every `put` moves the progress marker.
    pub fn tick(&mut self, now: Instant) {
        if self.frame.active && self.put_gen != self.seen_put_gen {
            // The open frame received bytes since the last check: that is progress.
            self.seen_put_gen = self.put_gen;
            return;
        }
        self.drop_stalled(now);
    }

    /// The stall check shared by `end` and `tick`.
    fn drop_stalled(&mut self, now: Instant) {
        if let Some(pending) = &self.pending {
            if now.saturating_duration_since(pending.last_progress) > STALL {
                let (id, number, quiet) = (pending.id.unwrap_or(0), pending.number, pending.quiet);
                self.pending = None;
                self.b64.reset();
                self.reject = self.reject.take().or(Some(KittyReject {
                    id,
                    number,
                    error: KittyError::ENOMEM,
                    quiet,
                }));
            }
        }
    }

    /// Takes the rejection waiting to be answered, if any. Rejections are kept one deep: a
    /// caller that calls `end` once per APC sequence sees every one.
    pub fn take_reject(&mut self) -> Option<KittyReject> {
        self.reject.take()
    }

    /// Bytes the parser is holding for transmissions in progress (test hook; the inflate state
    /// of a compressed transmission, under 64 KiB, is not counted). A dropped transmission
    /// holds nothing.
    pub fn held_bytes(&self) -> usize {
        let mut held = self.b64.carried() + self.frame.pair.len() + self.frame.text.len();
        if let Some(pending) = &self.pending {
            held += pending.out.len();
        }
        held
    }

    // --- control data ---------------------------------------------------------------------

    /// Read control text up to the first `;`; everything after it is payload.
    fn feed_control(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        loop {
            match rest.iter().position(|&b| b == b';' || b == b',') {
                Some(at) => {
                    if !self.extend_pair(&rest[..at]) {
                        return;
                    }
                    let ends_control = rest[at] == b';';
                    self.process_pair();
                    self.frame.pair.clear();
                    rest = &rest[at + 1..];
                    if ends_control {
                        // Payload only starts if everything up to the `;` was accepted.
                        if matches!(self.frame.region, Region::Control) {
                            self.frame.region = Region::Payload;
                            self.stage_payload(rest);
                        }
                        return;
                    }
                    if !matches!(self.frame.region, Region::Control) {
                        return; // `process_pair` rejected the frame
                    }
                },
                None => {
                    self.extend_pair(rest);
                    return;
                },
            }
        }
    }

    fn extend_pair(&mut self, segment: &[u8]) -> bool {
        if self.frame.pair.len() + segment.len() > PAIR_CAP {
            self.reject_frame(KittyError::EINVAL);
            false
        } else {
            self.frame.pair.extend_from_slice(segment);
            true
        }
    }

    /// Process one complete `key=value` pair.
    fn process_pair(&mut self) {
        if self.frame.pair.is_empty() {
            return; // leading, trailing or doubled commas: undefined per protocol, ignored here
        }
        let pair = std::mem::take(&mut self.frame.pair);
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            self.reject_frame(KittyError::EINVAL);
            return;
        };
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        if key.len() != 1 || !key[0].is_ascii_alphabetic() {
            self.reject_frame(KittyError::EINVAL);
            return;
        }
        // `i` and `I` together are an error for every action (protocol rule).
        let clash = (key[0] == b'i' && self.frame.keys.number.is_some())
            || (key[0] == b'I' && self.frame.keys.i.is_some());
        if clash {
            self.reject_frame(KittyError::EINVAL);
            return;
        }
        let keys = &mut self.frame.keys;
        let ok = match key[0] {
            b'a' => set(value, |c| "acdfpqtT".contains(c as char), |v| keys.a = Some(v)),
            b't' => set(value, |c| "dfts".contains(c as char), |v| keys.t = Some(v)),
            b'o' => set(value, |c| c == b'z', |v| keys.o = Some(v)),
            b'd' => set(value, is_selector, |v| keys.d = Some(v)),
            b'm' => bounded(value, 1, |n| keys.m = Some(n == 1)),
            b'q' => bounded(value, 2, |v| keys.q = Some(v)),
            b'f' | b'i' | b'I' | b'p' | b's' | b'v' | b'x' | b'y' | b'w' | b'h' | b'X' | b'Y'
            | b'c' | b'r' | b'C' | b'U' => uint(value).is_some_and(|n| {
                match key[0] {
                    b'f' => keys.f = Some(n),
                    b'i' => keys.i = (n != 0).then_some(n), // i=0 is the same as no id
                    b'I' => keys.number = (n != 0).then_some(n),
                    b'p' => keys.p = (n != 0).then_some(n), // placement ids start at 1
                    b's' => keys.s = Some(n),
                    b'v' => keys.v = Some(n),
                    b'x' => keys.x = Some(n),
                    b'y' => keys.y = Some(n),
                    b'w' => keys.w = Some(n),
                    b'h' => keys.h = Some(n),
                    b'X' => keys.x_off = Some(n),
                    b'Y' => keys.y_off = Some(n),
                    b'c' => keys.c = Some(n),
                    b'r' => keys.r = Some(n),
                    b'C' => keys.no_move = Some(n),
                    _ => keys.virtual_ = Some(n),
                }
                true
            }),
            b'z' => {
                if let Some(n) = int(value) {
                    keys.z = Some(n);
                    true
                } else {
                    false
                }
            },
            _ => false, // a key the protocol does not define here (`S`, `O`, `N`, `e`, ...)
        };
        if !ok {
            self.reject_frame(KittyError::EINVAL);
            return;
        }
        self.frame.used |= key_bit(key[0]);
    }

    fn reject_frame(&mut self, error: KittyError) {
        let frame = &mut self.frame;
        if frame.error.is_none() {
            frame.error = Some(error);
        }
        frame.region = Region::Skip;
        frame.pair = Vec::new();
        frame.text = Vec::new(); // the payload of a rejected frame is never staged further
    }

    // --- dispatch -------------------------------------------------------------------------

    fn dispatch(&mut self, frame: Frame, now: Instant, touched: &mut bool) -> Option<KittyCommand> {
        let keys = frame.keys;
        let continuation = self.pending.is_some()
            && frame.used & !(key_bit(b'm') | key_bit(b'q') | key_bit(b'i') | key_bit(b'I')) == 0
            && ids_match(&keys, self.pending.as_ref().unwrap());
        if continuation {
            *touched = true;
            // Continuation chunks may restate `q`; the last one sent answers the transmission.
            if let Some(q) = keys.q {
                self.pending.as_mut().unwrap().quiet = q;
            }
        } else {
            match keys.a.unwrap_or(b't') {
                b'd' => {
                    // Any delete aborts a chunked upload in progress (protocol rule).
                    *touched = true;
                    self.pending = None;
                    self.b64.reset();
                    let selector = keys.d.unwrap_or(b'a');
                    let target = match selector.to_ascii_lowercase() {
                        b'a' => DeleteTarget::All,
                        b'i' => DeleteTarget::ImageId { image: keys.i, placement: keys.p },
                        b'n' => DeleteTarget::Number { number: keys.number, placement: keys.p },
                        b'c' => DeleteTarget::CursorCells,
                        b'p' => {
                            DeleteTarget::Cell { x: keys.x.unwrap_or(0), y: keys.y.unwrap_or(0) }
                        },
                        b'q' => DeleteTarget::CellZ {
                            x: keys.x.unwrap_or(0),
                            y: keys.y.unwrap_or(0),
                            z: keys.z.unwrap_or(0),
                        },
                        b'x' => DeleteTarget::Column(keys.x.unwrap_or(0)),
                        b'y' => DeleteTarget::Row(keys.y.unwrap_or(0)),
                        b'z' => DeleteTarget::ZIndex(keys.z.unwrap_or(0)),
                        _ => DeleteTarget::IdRange {
                            from: keys.x.unwrap_or(0),
                            to: keys.y.unwrap_or(0),
                        },
                    };
                    return Some(KittyCommand::Delete(KittyDelete {
                        target,
                        free_data: selector.is_ascii_uppercase(),
                        quiet: keys.q.unwrap_or(0),
                    }));
                },
                b'q' => {
                    // The query action needs an id to answer for; it stores nothing and leaves
                    // any chunked upload alone. It must still refuse what a transmission would:
                    // clients use the query to choose a transfer medium, so answering OK for
                    // `t=f` would have them pick a medium every later image fails on.
                    let Some(id) = keys.i else {
                        self.reject_once(keys, KittyError::EINVAL);
                        return None;
                    };
                    if let Err(error) = plan_transmit(&keys) {
                        self.reject_once(keys, error);
                        return None;
                    }
                    return Some(KittyCommand::Query { id, quiet: keys.q.unwrap_or(0) });
                },
                b'p' => {
                    return Some(KittyCommand::Place(build_place(&keys, keys.q.unwrap_or(0))));
                },
                b'a' | b'c' | b'f' => {
                    // Animation is not supported here.
                    self.reject_once(keys, KittyError::ENOTSUPPORTED);
                    return None;
                },
                b't' | b'T' => {},
                _ => {
                    self.reject_once(keys, KittyError::EINVAL);
                    return None;
                },
            }

            // A new transmission silently replaces an unfinished one: the protocol requires
            // clients to finish all chunks of an image before sending anything else.
            if let Err(error) = plan_transmit(&keys) {
                self.reject_once(keys, error);
                return None;
            }
            let compressed = keys.o == Some(b'z');
            // `plan_transmit` validated the declared raw size, so it is safe to use as the
            // inflate ceiling for `o=z` data.
            let raw_limit = match (keys.f, keys.s, keys.v) {
                (Some(f @ (24 | 32)), Some(w), Some(h)) => {
                    let bpp: u64 = if f == 24 { 3 } else { 4 };
                    Some((u64::from(w) * u64::from(h) * bpp) as usize)
                },
                _ => None,
            };
            *touched = true;
            self.pending = Some(Pending {
                id: keys.i,
                number: keys.number,
                quiet: keys.q.unwrap_or(0),
                fmt_spec: keys.f,
                width_hint: keys.s,
                height_hint: keys.v,
                compressed,
                out: Vec::new(),
                inflator: compressed.then(|| Decompress::new(true)),
                fed: 0,
                stream_end: false,
                place: (keys.a == Some(b'T')).then(|| build_place(&keys, keys.q.unwrap_or(0))),
                raw_limit,
                last_progress: now,
            });
            self.b64.reset();
        }

        let more = frame.keys.m.unwrap_or(false);
        self.feed_payload(frame.text, more, now)
    }

    /// Decode this frame's staged text into the pending transmission and close it when `m` says
    /// so. Caps are enforced here — on the base64 length, before anything is decoded, so a frame
    /// that cannot fit is refused rather than turned into bytes first — and a killed transmission
    /// is freed before returning.
    fn feed_payload(&mut self, text: Vec<u8>, more: bool, now: Instant) -> Option<KittyCommand> {
        let Self { pending, b64, reject, .. } = self;
        // The room this frame's payload has left: the data cap for raw data, and the same cap on
        // the compressed bytes fed to the inflate for `o=z`, whose output `pump` caps separately. A
        // stream that already ended stores nothing its later chunks decode to, so nothing can pass
        // a cap there; invalid base64 — data after the payload's padding included — is still
        // refused.
        let room = match pending.as_ref().expect("feed_payload runs with a transmission") {
            p if p.stream_end => usize::MAX,
            p if p.compressed => MAX_PAYLOAD_BYTES.saturating_sub(p.fed as usize),
            p => MAX_PAYLOAD_BYTES.saturating_sub(p.out.len()),
        };
        let decoded = match b64.put(&text, room) {
            Ok(decoded) => decoded,
            Err(B64Error::TooBig) => {
                kill(reject, pending, b64, KittyError::ENOMEM);
                return None;
            },
            Err(B64Error::Invalid) => {
                kill(reject, pending, b64, KittyError::EINVAL);
                return None;
            },
        };
        let feed_result = {
            let p = pending.as_mut().expect("feed_payload runs with a transmission");
            if !p.compressed {
                // `put` measured this frame against the cap before decoding it, so these bytes fit.
                p.out.extend_from_slice(&decoded);
                Ok(())
            } else if p.stream_end {
                Ok(()) // trailing bytes after the zlib stream are ignored, and never stored
            } else {
                match p.fed.checked_add(decoded.len() as u64) {
                    Some(total) if total <= MAX_PAYLOAD_BYTES as u64 => {
                        p.fed = total;
                        let declared = p.declared_size();
                        pump(
                            p.inflator.as_mut().expect("compressed sets one"),
                            &decoded,
                            &mut p.out,
                            declared,
                        )
                        .map(|done| p.stream_end |= done)
                    },
                    _ => Err(KittyError::ENOMEM), // the compressed input passed the cap
                }
            }
        };
        if let Err(error) = feed_result {
            kill(reject, pending, b64, error);
            return None;
        }
        if more {
            pending.as_mut().unwrap().last_progress = now;
            return None;
        }

        // The transmission is complete: drain the last base64 group (at most two bytes), close the
        // inflate and validate. A stream that already ended ignores what its tail decodes to, and
        // for a compressed one `pump` bounds what those bytes can become.
        let tail_room = match pending.as_ref().expect("feed_payload runs with a transmission") {
            p if p.compressed => usize::MAX,
            p => MAX_PAYLOAD_BYTES.saturating_sub(p.out.len()),
        };
        let tail = match b64.finish(tail_room) {
            Ok(tail) => tail,
            Err(B64Error::TooBig) => {
                kill(reject, pending, b64, KittyError::ENOMEM);
                return None;
            },
            Err(B64Error::Invalid) => {
                kill(reject, pending, b64, KittyError::EINVAL);
                return None;
            },
        };
        let close_result = {
            let p = pending.as_mut().unwrap();
            if !p.compressed {
                // `finish` measured the tail against the cap before decoding it, so these bytes
                // fit.
                p.out.extend_from_slice(&tail);
                Ok(())
            } else if p.stream_end {
                // A finished zlib stream is complete: base64 that decodes past its end is not
                // image data and is dropped, not appended.
                Ok(())
            } else if p.fed == 0 && tail.is_empty() {
                Ok(()) // nothing was ever fed to the decompressor: an empty transmission
            } else {
                let declared = p.declared_size();
                let inflator = p.inflator.as_mut().expect("compressed sets one");
                match pump(inflator, &tail, &mut p.out, declared) {
                    Ok(true) => Ok(true),
                    Ok(false) => {
                        pump_with(inflator, &[], &mut p.out, FlushDecompress::Finish, declared)
                    },
                    Err(error) => Err(error),
                }
                .and_then(|done| if done { Ok(()) } else { Err(KittyError::EINVAL) }) // truncated stream
            }
        };
        if let Err(error) = close_result {
            kill(reject, pending, b64, error);
            return None;
        }
        let resolved = {
            let p = pending.as_mut().unwrap();
            if p.out.is_empty() {
                // No payload: nothing to store or place, and no reply.
                Ok(None)
            } else {
                resolve_format(p.fmt_spec, p.width_hint, p.height_hint, &p.out).map(Some)
            }
        };
        let format = match resolved {
            Ok(Some(format)) => format,
            Ok(None) => {
                *pending = None;
                b64.reset();
                return None;
            },
            Err(error) => {
                kill(reject, pending, b64, error);
                return None;
            },
        };
        let command = {
            let p = pending.as_mut().unwrap();
            KittyCommand::Transmit {
                id: p.id,
                number: p.number,
                format,
                bytes: std::mem::take(&mut p.out),
                then_place: p.place.take(),
                quiet: p.quiet,
            }
        };
        *pending = None; // releases what the inflate did not move into the command
        b64.reset();
        Some(command)
    }

    fn reject_once(&mut self, keys: Keys, error: KittyError) {
        self.reject = self.reject.take().or(Some(KittyReject {
            id: keys.i.unwrap_or(0),
            number: keys.number,
            error,
            quiet: keys.q.unwrap_or(0),
        }));
    }
}

fn ids_match(keys: &Keys, pending: &Pending) -> bool {
    keys.i.is_none_or(|i| Some(i) == pending.id)
        && keys.number.is_none_or(|n| Some(n) == pending.number)
}

/// Validate the control data of a new transmission. Returns the declared format (`None` means
/// sniff PNG magic at completion); raw formats need usable dimensions right away.
fn plan_transmit(keys: &Keys) -> Result<(), KittyError> {
    if matches!(keys.t.unwrap_or(b'd'), b'f' | b't' | b's') {
        return Err(KittyError::EBADF); // files are never read here
    }
    match keys.f {
        Some(24) | Some(32) => {
            let (w, h) = match (keys.s, keys.v) {
                (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
                _ => return Err(KittyError::EINVAL), // raw data needs its dimensions
            };
            let bpp = if keys.f == Some(24) { 3 } else { 4 };
            checked_size(w, h, bpp)?;
        },
        Some(8 | 16) => return Err(KittyError::ENOTSUPPORTED),
        Some(100) | None => {},
        Some(_) => return Err(KittyError::EINVAL),
    }
    Ok(())
}

/// Sides within `MAX_IMAGE_SIDE` and `width * height * bpp` within `MAX_IMAGE_BYTES`.
fn checked_size(w: u32, h: u32, bpp: u64) -> Result<(), KittyError> {
    if w > MAX_IMAGE_SIDE || h > MAX_IMAGE_SIDE {
        return Err(KittyError::ENOMEM);
    }
    if u64::from(w) * u64::from(h) * bpp > MAX_IMAGE_BYTES {
        return Err(KittyError::ENOMEM);
    }
    Ok(())
}

/// Resolve and validate the payload: sniff PNG when no format was declared, read PNG sizes from
/// the header, and require raw data to be exactly its declared size.
fn resolve_format(
    fmt_spec: Option<u32>,
    width_hint: Option<u32>,
    height_hint: Option<u32>,
    out: &[u8],
) -> Result<KittyFormat, KittyError> {
    let is_png = out.starts_with(PNG_MAGIC);
    if fmt_spec == Some(100) || (fmt_spec.is_none() && is_png) {
        if !is_png {
            return Err(KittyError::EBADPNG);
        }
        let size = imagesize::blob_size(out).map_err(|_| KittyError::EBADPNG)?;
        let (Ok(width), Ok(height)) = (u32::try_from(size.width), u32::try_from(size.height))
        else {
            return Err(KittyError::ENOMEM);
        };
        checked_size(width, height, 4)?; // the embedder decodes PNG to RGBA
        return Ok(KittyFormat::Png { width, height });
    }
    let fmt = fmt_spec.unwrap_or(32); // without `f`, non-PNG data is RGBA (the protocol default)
    let bpp = if fmt == 24 { 3 } else { 4 };
    let (Some(w), Some(h)) = (width_hint.filter(|w| *w > 0), height_hint.filter(|h| *h > 0)) else {
        return Err(KittyError::EINVAL); // raw data needs its dimensions
    };
    checked_size(w, h, bpp)?;
    let expected = u64::from(w) * u64::from(h) * bpp;
    if out.len() as u64 != expected {
        return Err(KittyError::EINVAL); // short or long pixel data
    }
    Ok(if fmt == 24 {
        KittyFormat::Rgb { width: w, height: h }
    } else {
        KittyFormat::Rgba { width: w, height: h }
    })
}

fn build_place(keys: &Keys, quiet: u8) -> KittyPlace {
    let src =
        (keys.x.is_some() || keys.y.is_some() || keys.w.is_some() || keys.h.is_some()).then(|| {
            Rect {
                x: keys.x.unwrap_or(0),
                y: keys.y.unwrap_or(0),
                w: keys.w.unwrap_or(0),
                h: keys.h.unwrap_or(0),
            }
        });
    KittyPlace {
        id: keys.i,
        number: keys.number,
        placement_id: keys.p,
        // A hostile `c`/`r` cannot ask for a span wider than the image limits allow; `0`
        // (absent) stays `0` so `PlacementTable::span_for` computes it.
        cols: keys.c.unwrap_or(0).min(MAX_IMAGE_SIDE),
        rows: keys.r.unwrap_or(0).min(MAX_IMAGE_SIDE),
        src,
        x_off: keys.x_off.unwrap_or(0),
        y_off: keys.y_off.unwrap_or(0),
        z: keys.z.unwrap_or(0),
        virtual_: keys.virtual_.unwrap_or(0) != 0,
        cursor_movement: keys.no_move.unwrap_or(0) == 0, // C=1 is "no cursor movement"
        quiet,
    }
}

// --- payload plumbing ---------------------------------------------------------------------

/// Abandon the pending transmission with `error`: its accumulated data is released, the
/// rejection is recorded (an earlier one in the same `end` wins), and the base64 carry is reset.
fn kill(
    reject: &mut Option<KittyReject>,
    pending: &mut Option<Pending>,
    b64: &mut Decoder,
    error: KittyError,
) {
    if let Some(pending) = pending.take() {
        if reject.is_none() {
            *reject = Some(KittyReject {
                id: pending.id.unwrap_or(0),
                number: pending.number,
                error,
                quiet: pending.quiet,
            });
        }
    }
    b64.reset();
}

/// Inflate `input` into `out`, stopping at the stream end. Errors on corrupt data (`EINVAL`), once
/// the output would pass `MAX_IMAGE_BYTES` (`ENOMEM` — checked first, so a bomb is reported as too
/// much image data), or once it passes the `declared` byte count of a raw format (`EINVAL`: more
/// pixel data than the sender described, and the excess is never inflated). Returns whether the
/// stream ended.
fn pump(
    inflator: &mut Decompress,
    input: &[u8],
    out: &mut Vec<u8>,
    declared: Option<u64>,
) -> Result<bool, KittyError> {
    pump_with(inflator, input, out, FlushDecompress::None, declared)
}

fn pump_with(
    inflator: &mut Decompress,
    input: &[u8],
    out: &mut Vec<u8>,
    flush: FlushDecompress,
    declared: Option<u64>,
) -> Result<bool, KittyError> {
    let mut spare = vec![0u8; INFLATE_SPARE];
    let mut pos = 0usize;
    loop {
        let in_before = inflator.total_in();
        let out_before = inflator.total_out();
        let status = inflator
            .decompress(&input[pos..], &mut spare[..], flush)
            .map_err(|_| KittyError::EINVAL)?;
        let consumed = (inflator.total_in() - in_before) as usize;
        let produced = (inflator.total_out() - out_before) as usize;
        pos += consumed;
        if produced > 0 {
            let total = out.len() as u64 + produced as u64;
            if total > MAX_IMAGE_BYTES {
                return Err(KittyError::ENOMEM);
            }
            if declared.is_some_and(|declared| total > declared) {
                return Err(KittyError::EINVAL); // more pixel data than the sender declared
            }
            out.extend_from_slice(&spare[..produced]);
        }
        match status {
            Status::StreamEnd => return Ok(true),
            Status::Ok | Status::BufError => {
                if consumed == 0 && produced == 0 {
                    return Ok(false); // needs more input (or the stream is not over)
                }
            },
        }
    }
}

// --- small parsers ------------------------------------------------------------------------

/// A one-character value, kept when it passes `check`.
fn set(value: &[u8], check: impl FnOnce(u8) -> bool, store: impl FnOnce(u8)) -> bool {
    match value {
        &[c] if check(c) => {
            store(c);
            true
        },
        _ => false,
    }
}

/// An unsigned 32-bit decimal value (plain digits only).
fn uint(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// A signed 32-bit decimal value (plain digits with an optional leading minus).
fn int(value: &[u8]) -> Option<i32> {
    let digits = value.strip_prefix(b"-").unwrap_or(value);
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn bounded(value: &[u8], max: u32, store: impl FnOnce(u8)) -> bool {
    match uint(value) {
        Some(n) if n <= max => {
            store(n as u8);
            true
        },
        _ => false,
    }
}

fn is_selector(c: u8) -> bool {
    matches!(
        c.to_ascii_lowercase(),
        b'a' | b'i' | b'n' | b'c' | b'p' | b'q' | b'x' | b'y' | b'z' | b'r'
    )
}

/// One bit per control key (the 26 keys this parser accepts; anything else never reaches here).
const fn key_bit(key: u8) -> u64 {
    match key {
        b'a' => 1 << 0,
        b't' => 1 << 1,
        b'f' => 1 << 2,
        b'o' => 1 << 3,
        b'm' => 1 << 4,
        b'i' => 1 << 5,
        b'I' => 1 << 6,
        b'p' => 1 << 7,
        b'q' => 1 << 8,
        b's' => 1 << 9,
        b'v' => 1 << 10,
        b'x' => 1 << 11,
        b'y' => 1 << 12,
        b'w' => 1 << 13,
        b'h' => 1 << 14,
        b'X' => 1 << 15,
        b'Y' => 1 << 16,
        b'c' => 1 << 17,
        b'r' => 1 << 18,
        b'C' => 1 << 19,
        b'z' => 1 << 20,
        b'U' => 1 << 21,
        b'd' => 1 << 22,
        _ => 1 << 63,
    }
}
