// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

use std::borrow::Cow;
use std::fmt::{self, Debug, Formatter};
use std::process::ExitStatus;
use std::sync::Arc;

use crate::term::ClipboardType;
use crate::vte::ansi::Rgb;

/// Identifies one image the terminal handed to the embedder.
///
/// Keys are handed out in increasing order by [`crate::term::Term`] and each names exactly one
/// image: the payload of [`Event::ImageReceived`] and the subject of [`Event::ImageReleased`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageKey(pub u64);

/// An encoded image format the embedder is asked to decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// PNG data.
    Png,
    /// JPEG data.
    Jpeg,
    /// GIF data.
    Gif,
}

/// The pixels or encoded bytes of one image, as [`Event::ImageReceived`] delivers them.
///
/// `Rgba` carries straight (non-premultiplied) 8-bit RGBA with `pixels.len()` equal to
/// `width * height * 4`; the terminal converts its own pixel formats (sixel output, raw RGB) to
/// that layout. `Encoded` carries a container format's bytes for the embedder to decode;
/// `width`/`height` are the dimensions read from the header or declared by the sender, so
/// placements can be laid out before decoding finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageData {
    /// Decoded pixels: `pixels.len() == width * height * 4`.
    Rgba { width: u32, height: u32, pixels: Vec<u8> },
    /// Encoded image bytes with the dimensions their header declares.
    Encoded { format: ImageFormat, bytes: Vec<u8>, width: u32, height: u32 },
}

/// Why an image the embedder was asked to decode is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageDecodeError {
    /// The payload is not a valid image in its format.
    Corrupt,
    /// Decoding would exceed the terminal's image size limits.
    TooLarge,
}

/// Terminal event.
///
/// These events instruct the UI over changes that can't be handled by the terminal emulation layer
/// itself.
#[derive(Clone)]
pub enum Event {
    /// Grid has changed possibly requiring a mouse cursor shape change.
    MouseCursorDirty,

    /// Window title change.
    Title(String),

    /// Reset to the default window title.
    ResetTitle,

    /// Request to store a text string in the clipboard.
    ClipboardStore(ClipboardType, String),

    /// Request to write the contents of the clipboard to the PTY.
    ///
    /// The attached function is a formatter which will correctly transform the clipboard content
    /// into the expected escape sequence format.
    ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String + Sync + Send + 'static>),

    /// Request to write the RGB value of a color to the PTY.
    ///
    /// The attached function is a formatter which will correctly transform the RGB color into the
    /// expected escape sequence format.
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>),

    /// Write some text to the PTY.
    PtyWrite(String),

    /// Request to write the text area size.
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>),

    /// Cursor blinking state has changed.
    CursorBlinkingChange,

    /// New terminal content available.
    Wakeup,

    /// Terminal bell ring.
    Bell,

    /// Shutdown request.
    Exit,

    /// Child process exited.
    ChildExit(ExitStatus),

    /// An image's payload arrived and is the embedder's to decode and draw (feathercritty).
    ///
    /// The key names this payload from then on; [`Term::image_decoded`] reports the decode result
    /// back to the terminal, which answers any client waiting for it.
    ///
    /// [`Term::image_decoded`]: crate::term::Term::image_decoded
    ImageReceived { key: ImageKey, data: ImageData },

    /// Nothing references this image any more and the embedder should free it (feathercritty).
    ///
    /// Sent exactly once per key, when its last placement, placeholder or stored reference is
    /// gone; a key that was released is never reused.
    ImageReleased(ImageKey),
}

impl Debug for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Event::ClipboardStore(ty, text) => write!(f, "ClipboardStore({ty:?}, {text})"),
            Event::ClipboardLoad(ty, _) => write!(f, "ClipboardLoad({ty:?})"),
            Event::TextAreaSizeRequest(_) => write!(f, "TextAreaSizeRequest"),
            Event::ColorRequest(index, _) => write!(f, "ColorRequest({index})"),
            Event::PtyWrite(text) => write!(f, "PtyWrite({text})"),
            Event::Title(title) => write!(f, "Title({title})"),
            Event::CursorBlinkingChange => write!(f, "CursorBlinkingChange"),
            Event::MouseCursorDirty => write!(f, "MouseCursorDirty"),
            Event::ResetTitle => write!(f, "ResetTitle"),
            Event::Wakeup => write!(f, "Wakeup"),
            Event::Bell => write!(f, "Bell"),
            Event::Exit => write!(f, "Exit"),
            Event::ChildExit(status) => write!(f, "ChildExit({status:?})"),
            // The payload can be megabytes; the key is what identifies it in a log.
            Event::ImageReceived { key, data } => match data {
                ImageData::Rgba { width, height, pixels } => write!(
                    f,
                    "ImageReceived({key:?}, Rgba {{ {width}x{height}, {} bytes }})",
                    pixels.len()
                ),
                ImageData::Encoded { format, bytes, width, height } => write!(
                    f,
                    "ImageReceived({key:?}, Encoded {{ {format:?} {width}x{height}, {} bytes }})",
                    bytes.len()
                ),
            },
            Event::ImageReleased(key) => write!(f, "ImageReleased({key:?})"),
        }
    }
}

/// Byte sequences are sent to a `Notify` in response to some events.
pub trait Notify {
    /// Notify that an escape sequence should be written to the PTY.
    ///
    /// TODO this needs to be able to error somehow.
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, _: B);
}

#[derive(Copy, Clone, Debug)]
pub struct WindowSize {
    pub num_lines: u16,
    pub num_cols: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// Types that are interested in when the display is resized.
pub trait OnResize {
    fn on_resize(&mut self, window_size: WindowSize);
}

/// Event Loop for notifying the renderer about terminal events.
pub trait EventListener {
    fn send_event(&self, _event: Event) {}
}

/// Null sink for events.
pub struct VoidListener;

impl EventListener for VoidListener {}
