//! Client-to-server and server-to-client messages (RFC 6143 sections 7.5
//! and 7.6), plus the extensions every modern client speaks: continuous
//! updates, fences, desktop resize, QEMU key events and the extended
//! clipboard.
//!
//! [`parse_client`] is incremental: it is given whatever bytes have arrived
//! and answers with a message and how many bytes it used, or `None` when the
//! message is not complete yet. Lengths are checked against a ceiling before
//! any wait, so a hostile length cannot make a session buffer for ever.

use crate::{Error, PixelFormat};

pub mod client_type {
    pub const SET_PIXEL_FORMAT: u8 = 0;
    pub const SET_ENCODINGS: u8 = 2;
    pub const FRAMEBUFFER_UPDATE_REQUEST: u8 = 3;
    pub const KEY_EVENT: u8 = 4;
    pub const POINTER_EVENT: u8 = 5;
    pub const CLIENT_CUT_TEXT: u8 = 6;
    pub const ENABLE_CONTINUOUS_UPDATES: u8 = 150;
    pub const CLIENT_FENCE: u8 = 248;
    pub const SET_DESKTOP_SIZE: u8 = 251;
    pub const QEMU: u8 = 255;
}

pub mod server_type {
    pub const FRAMEBUFFER_UPDATE: u8 = 0;
    pub const SET_COLOUR_MAP_ENTRIES: u8 = 1;
    pub const BELL: u8 = 2;
    pub const SERVER_CUT_TEXT: u8 = 3;
    pub const END_OF_CONTINUOUS_UPDATES: u8 = 150;
    pub const SERVER_FENCE: u8 = 248;
}

/// Fence flags. A client sets REQUEST; the server answers with the same
/// payload and the other flags once it has honoured them.
pub const FENCE_BLOCK_BEFORE: u32 = 1 << 0;
pub const FENCE_BLOCK_AFTER: u32 = 1 << 1;
pub const FENCE_SYNC_NEXT: u32 = 1 << 2;
pub const FENCE_REQUEST: u32 = 1 << 31;

pub const MAX_CUT_TEXT: usize = 1 << 20;
pub const MAX_ENCODINGS: usize = 1024;
pub const MAX_FENCE_PAYLOAD: usize = 64;
pub const MAX_SCREENS: usize = 16;

/// One screen of an ExtendedDesktopSize layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub id: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    FramebufferUpdateRequest {
        incremental: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    KeyEvent {
        down: bool,
        keysym: u32,
    },
    PointerEvent {
        buttons: u8,
        x: u16,
        y: u16,
    },
    /// Latin-1 text, decoded.
    ClientCutText(String),
    /// An Extended Clipboard message (negative length in ClientCutText).
    ExtendedClipboard(ClipboardMessage),
    /// An Extended Clipboard body this build could not read, with whatever
    /// its flag word was.
    ///
    /// A message rather than an error on purpose. The far side is still
    /// speaking RFB correctly and everything after this will parse; a
    /// clipboard that will not decode is worth a line in a log and nothing
    /// more. Hanging up on it drops somebody's whole session over a paste,
    /// which is what happened the first time noVNC pasted into this server.
    UnreadableClipboard {
        flags: u32,
        why: Error,
    },
    EnableContinuousUpdates {
        enable: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    ClientFence {
        flags: u32,
        payload: Vec<u8>,
    },
    SetDesktopSize {
        width: u16,
        height: u16,
        screens: Vec<Screen>,
    },
    /// QEMU Extended Key Event: the keysym plus the raw XT scancode.
    QemuKeyEvent {
        down: bool,
        keysym: u32,
        keycode: u32,
    },
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
}

/// Parse one client message from the front of `buf`.
///
/// `Ok(None)` means the message is incomplete: keep the bytes, read more,
/// call again. `Ok(Some((message, used)))` consumed `used` bytes.
pub fn parse_client(buf: &[u8]) -> Result<Option<(ClientMessage, usize)>, Error> {
    macro_rules! need {
        ($e:expr) => {
            match $e {
                Some(v) => v,
                None => return Ok(None),
            }
        };
    }

    let mut c = Cursor::new(buf);
    let kind = need!(c.u8());
    let msg = match kind {
        client_type::SET_PIXEL_FORMAT => {
            need!(c.skip(3));
            let raw = need!(c.take(PixelFormat::WIRE_LEN));
            ClientMessage::SetPixelFormat(PixelFormat::parse(raw)?)
        }
        client_type::SET_ENCODINGS => {
            need!(c.skip(1));
            let n = usize::from(need!(c.u16()));
            if n > MAX_ENCODINGS {
                return Err(Error::TooLong {
                    what: "encoding list",
                    len: n,
                    limit: MAX_ENCODINGS,
                });
            }
            let raw = need!(c.take(n * 4));
            let list = raw
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| i32::from_be_bytes(*b))
                .collect();
            ClientMessage::SetEncodings(list)
        }
        client_type::FRAMEBUFFER_UPDATE_REQUEST => {
            let incremental = need!(c.u8()) != 0;
            let x = need!(c.u16());
            let y = need!(c.u16());
            let width = need!(c.u16());
            let height = need!(c.u16());
            ClientMessage::FramebufferUpdateRequest {
                incremental,
                x,
                y,
                width,
                height,
            }
        }
        client_type::KEY_EVENT => {
            let down = need!(c.u8()) != 0;
            need!(c.skip(2));
            let keysym = need!(c.u32());
            ClientMessage::KeyEvent { down, keysym }
        }
        client_type::POINTER_EVENT => {
            let buttons = need!(c.u8());
            let x = need!(c.u16());
            let y = need!(c.u16());
            ClientMessage::PointerEvent { buttons, x, y }
        }
        client_type::CLIENT_CUT_TEXT => {
            need!(c.skip(3));
            let len = need!(c.u32());
            // The Extended Clipboard extension reuses this message with a
            // negative length; the magnitude is the payload size.
            let extended = (len as i32) < 0;
            let n = (len as i32).unsigned_abs() as usize;
            if n > MAX_CUT_TEXT {
                return Err(Error::TooLong {
                    what: "cut text",
                    len: n,
                    limit: MAX_CUT_TEXT,
                });
            }
            let raw = need!(c.take(n));
            if extended {
                match parse_clipboard(raw) {
                    Ok(message) => ClientMessage::ExtendedClipboard(message),
                    Err(why) => ClientMessage::UnreadableClipboard {
                        flags: raw.first_chunk::<4>().map_or(0, |b| u32::from_be_bytes(*b)),
                        why,
                    },
                }
            } else {
                ClientMessage::ClientCutText(latin1_to_string(raw))
            }
        }
        client_type::ENABLE_CONTINUOUS_UPDATES => {
            let enable = need!(c.u8()) != 0;
            let x = need!(c.u16());
            let y = need!(c.u16());
            let width = need!(c.u16());
            let height = need!(c.u16());
            ClientMessage::EnableContinuousUpdates {
                enable,
                x,
                y,
                width,
                height,
            }
        }
        client_type::CLIENT_FENCE => {
            need!(c.skip(3));
            let flags = need!(c.u32());
            let n = usize::from(need!(c.u8()));
            if n > MAX_FENCE_PAYLOAD {
                return Err(Error::TooLong {
                    what: "fence payload",
                    len: n,
                    limit: MAX_FENCE_PAYLOAD,
                });
            }
            let payload = need!(c.take(n)).to_vec();
            ClientMessage::ClientFence { flags, payload }
        }
        client_type::SET_DESKTOP_SIZE => {
            need!(c.skip(1));
            let width = need!(c.u16());
            let height = need!(c.u16());
            let n = usize::from(need!(c.u8()));
            need!(c.skip(1));
            if n > MAX_SCREENS {
                return Err(Error::TooLong {
                    what: "screen list",
                    len: n,
                    limit: MAX_SCREENS,
                });
            }
            let mut screens = Vec::with_capacity(n);
            for _ in 0..n {
                let id = need!(c.u32());
                let x = need!(c.u16());
                let y = need!(c.u16());
                let width = need!(c.u16());
                let height = need!(c.u16());
                let flags = need!(c.u32());
                screens.push(Screen {
                    id,
                    x,
                    y,
                    width,
                    height,
                    flags,
                });
            }
            ClientMessage::SetDesktopSize {
                width,
                height,
                screens,
            }
        }
        client_type::QEMU => {
            let subtype = need!(c.u8());
            match subtype {
                0 => {
                    let down = need!(c.u16()) != 0;
                    let keysym = need!(c.u32());
                    let keycode = need!(c.u32());
                    ClientMessage::QemuKeyEvent {
                        down,
                        keysym,
                        keycode,
                    }
                }
                other => return Err(Error::UnknownQemuSubtype(other)),
            }
        }
        other => return Err(Error::UnknownMessageType(other)),
    };
    Ok(Some((msg, c.pos)))
}

/// Cut text is Latin-1 on the wire: one byte per character, straight to a char.
pub fn latin1_to_string(raw: &[u8]) -> String {
    raw.iter().map(|&b| char::from(b)).collect()
}

/// Back to Latin-1; anything outside it becomes a question mark.
pub fn string_to_latin1(text: &str) -> Vec<u8> {
    text.chars()
        .map(|ch| u8::try_from(u32::from(ch)).unwrap_or(b'?'))
        .collect()
}

// The Extended Clipboard extension, pseudo-encoding 0xc0a1e5ce.
//
// The plain cut-text message says one thing and says it badly: a Latin-1
// string, pushed at the far side whether or not anybody there wants it. The
// extension reuses the same message with the length written negative, and
// puts a small protocol in the body. Each side opens with what it can take
// (caps). After that a copy is announced (notify), fetched when somebody
// actually pastes (request), and carried UTF-8 and compressed (provide). So
// a megabyte on the clipboard costs four bytes until it is wanted.

/// The bits an Extended Clipboard body opens with.
pub mod clipboard {
    /// The action bits. Every message but caps carries exactly one; a caps
    /// message sets its own bit plus one for every action it will answer.
    pub mod action {
        pub const CAPS: u32 = 1 << 24;
        pub const REQUEST: u32 = 1 << 25;
        pub const PEEK: u32 = 1 << 26;
        pub const NOTIFY: u32 = 1 << 27;
        pub const PROVIDE: u32 = 1 << 28;
        /// Everything this build knows how to answer.
        pub const ALL: u32 = CAPS | REQUEST | PEEK | NOTIFY | PROVIDE;
    }

    /// The format bits, lowest first, which is the order their sizes appear
    /// in a caps body and their data in a provide.
    pub mod format {
        pub const TEXT: u32 = 1 << 0;
        pub const RTF: u32 = 1 << 1;
        pub const HTML: u32 = 1 << 2;
        pub const DIB: u32 = 1 << 3;
        pub const FILES: u32 = 1 << 4;
        /// The five that are defined. Bits 5 to 23 are reserved, and a peer
        /// that sets one is answered by stepping over it, not by refusing.
        pub const ALL: u32 = TEXT | RTF | HTML | DIB | FILES;
        /// Every bit a format may occupy, known or not. A provide carries a
        /// length and a block for each one set, so the walk over a body has
        /// to cover all of them or it loses its place.
        pub const EVERY: u32 = 0x00ff_ffff;
    }
}

/// One Extended Clipboard message, whichever way it was going.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardMessage {
    /// What the far side can take, and the most it will accept of each
    /// format it named, lowest format bit first.
    Caps {
        formats: u32,
        actions: u32,
        sizes: Vec<u32>,
    },
    /// Send what is held, in any of these formats.
    Request(u32),
    /// Say what is held, without sending it.
    Peek(u32),
    /// Something was copied over there, and these formats are available.
    Notify(u32),
    /// Here it is. Only text is unpacked; another format is stepped over
    /// rather than refused.
    Provide { text: Option<String> },
    /// An action this build does not know. The extension is meant to grow,
    /// so one of those is something to ignore rather than to hang up over.
    Unknown(u32),
}

impl ClipboardMessage {
    /// Whether a peer that sent this caps message will answer `action`.
    pub fn answers(&self, action: u32) -> bool {
        match self {
            ClipboardMessage::Caps { actions, .. } => actions & action != 0,
            _ => false,
        }
    }
}

/// Read an Extended Clipboard body: the bytes after the negative length.
pub fn parse_clipboard(body: &[u8]) -> Result<ClipboardMessage, Error> {
    let head = body
        .first_chunk::<4>()
        .ok_or(Error::Truncated("extended clipboard"))?;
    let flags = u32::from_be_bytes(*head);
    let rest = &body[4..];
    let formats = flags & clipboard::format::ALL;
    let actions = flags & clipboard::action::ALL;

    // Caps is the one message with more than one action bit set, so it is
    // tested for rather than matched on.
    if actions & clipboard::action::CAPS != 0 {
        // One size per format bit the peer named, in bit order. A peer
        // naming a format this build has never heard of still owes its four
        // bytes, so the count is over every bit set below 24.
        let named = (flags & clipboard::format::EVERY).count_ones() as usize;
        let want = named * 4;
        if rest.len() < want {
            return Err(Error::Truncated("extended clipboard caps"));
        }
        let sizes = rest[..want]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_be_bytes(*b))
            .collect();
        return Ok(ClipboardMessage::Caps {
            formats,
            actions,
            sizes,
        });
    }

    match actions {
        clipboard::action::REQUEST => Ok(ClipboardMessage::Request(formats)),
        clipboard::action::PEEK => Ok(ClipboardMessage::Peek(formats)),
        clipboard::action::NOTIFY => Ok(ClipboardMessage::Notify(formats)),
        clipboard::action::PROVIDE => Ok(ClipboardMessage::Provide {
            text: provided_text(flags, rest)?,
        }),
        _ => Ok(ClipboardMessage::Unknown(flags)),
    }
}

/// Inflate a provide body and pull the text out of it, if it holds any.
fn provided_text(flags: u32, rest: &[u8]) -> Result<Option<String>, Error> {
    use std::io::Read;

    // Read a block at a time and never to the end of the stream. Every
    // writer in the wild flushes its deflater rather than finishing it,
    // because the same deflater carries on into the next message: there is
    // no final block and no Adler-32 trailer, and asking for the end
    // reports a truncation that is not one. The lengths embedded in the
    // data say how much there is, which is what TigerVNC's reader goes by
    // and all the extension actually promises. noVNC pastes died here.
    let mut z = flate2::read::ZlibDecoder::new(rest);
    let mut text = None;
    // A clipboard that inflates to more than the plain message's limit is
    // not a clipboard, and the budget is over the whole body rather than
    // each format: twenty-four megabyte-long blocks out of a few hundred
    // compressed bytes is a cheap way to ask for memory.
    let mut budget = MAX_CUT_TEXT;
    for bit in 0..24u32 {
        if flags & (1 << bit) == 0 {
            continue;
        }
        let mut head = [0u8; 4];
        z.read_exact(&mut head).map_err(inflate_trouble)?;
        let len = u32::from_be_bytes(head) as usize;
        if len > budget {
            return Err(Error::TooLong {
                what: "extended clipboard provide",
                len,
                limit: MAX_CUT_TEXT,
            });
        }
        budget -= len;
        // Read even for a format this build does not keep: the blocks are
        // back to back, so stepping over one means reading past it.
        let mut data = vec![0u8; len];
        z.read_exact(&mut data).map_err(inflate_trouble)?;
        if 1 << bit == clipboard::format::TEXT {
            text = Some(clipboard_text(&data));
        }
    }
    Ok(text)
}

/// Tell a stream that stops early from one that is not a stream at all.
///
/// The first is a peer that said it would send more than it did, which is a
/// message to drop; the second is noise, which is the same. They are kept
/// apart because the log line is the first thing anybody reads when a
/// clipboard stops working.
fn inflate_trouble(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::Truncated("extended clipboard provide")
    } else {
        Error::Clipboard("the provide does not inflate")
    }
}

/// The extension's text format: UTF-8, line ends LF, and a terminating null
/// counted in the length.
///
/// The null is dropped if it is there and not missed if it is not, and bytes
/// that are not UTF-8 are replaced rather than refused: a clipboard arriving
/// with a question mark in it beats one that does not arrive.
fn clipboard_text(data: &[u8]) -> String {
    let body = data.strip_suffix(b"\0").unwrap_or(data);
    String::from_utf8_lossy(body).replace("\r\n", "\n")
}

/// A caps body: every format and action this side will answer, then the most
/// it will take of each format, lowest bit first.
pub fn clipboard_caps(formats: u32, actions: u32, sizes: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + sizes.len() * 4);
    let flags = formats | actions | clipboard::action::CAPS;
    body.extend_from_slice(&flags.to_be_bytes());
    for size in sizes {
        body.extend_from_slice(&size.to_be_bytes());
    }
    body
}

/// A request, peek or notify body, which is the flags and nothing else.
pub fn clipboard_flags(action: u32, formats: u32) -> Vec<u8> {
    (action | formats).to_be_bytes().to_vec()
}

/// A provide body carrying text: the flags, then one zlib stream holding a
/// length and the bytes for each format named.
pub fn clipboard_provide_text(text: &str) -> Vec<u8> {
    use std::io::Write;

    let body = (clipboard::action::PROVIDE | clipboard::format::TEXT)
        .to_be_bytes()
        .to_vec();
    let flat = provide_block(text);
    // Level 6 rather than the 1 the framebuffer's streams use. A clipboard
    // is kilobytes and happens when somebody presses a key, so the time the
    // extra levels cost is time nothing else wanted.
    let mut z = flate2::write::ZlibEncoder::new(body, flate2::Compression::new(6));
    z.write_all(&flat).expect("a Vec takes every byte");
    z.finish().expect("a Vec takes every byte")
}

/// The same body with the stream flushed rather than finished.
///
/// What every writer in the wild produces: noVNC's Deflator and TigerVNC's
/// ZlibOutStream both flush and stop, because the same deflater carries on
/// into the next message. Here so the tests can send that shape. Nothing in
/// the server sends it, because a finished stream reads correctly for both
/// kinds of reader and a flushed one only reads for the careful kind.
pub fn clipboard_provide_text_flushed(text: &str) -> Vec<u8> {
    let mut body = (clipboard::action::PROVIDE | clipboard::format::TEXT)
        .to_be_bytes()
        .to_vec();
    let mut z = flate2::Compress::new(flate2::Compression::new(6), true);
    let mut out = vec![0u8; text.len() + 64];
    let flat = provide_block(text);
    z.compress(&flat, &mut out, flate2::FlushCompress::Sync)
        .expect("a buffer with room in it");
    let wrote = z.total_out() as usize;
    body.extend_from_slice(&out[..wrote]);
    body
}

/// One format's block inside a provide: a length, the text, and the null
/// the length counts.
fn provide_block(text: &str) -> Vec<u8> {
    let utf8 = text.replace("\r\n", "\n");
    let mut flat = Vec::with_capacity(utf8.len() + 5);
    flat.extend_from_slice(&((utf8.len() + 1) as u32).to_be_bytes());
    flat.extend_from_slice(utf8.as_bytes());
    flat.push(0);
    flat
}

/// Wrap an Extended Clipboard body in a ServerCutText, with the negative
/// length that tells it apart from a plain one.
pub fn write_server_clipboard(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&[server_type::SERVER_CUT_TEXT, 0, 0, 0]);
    out.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
    out.extend_from_slice(body);
}

/// The same, going the other way.
pub fn write_client_clipboard(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&[client_type::CLIENT_CUT_TEXT, 0, 0, 0]);
    out.extend_from_slice(&(-(body.len() as i32)).to_be_bytes());
    out.extend_from_slice(body);
}

// Client message writers: the test client's half, and what the parser tests
// round-trip through.

pub fn write_set_pixel_format(out: &mut Vec<u8>, pf: &PixelFormat) {
    out.extend_from_slice(&[client_type::SET_PIXEL_FORMAT, 0, 0, 0]);
    pf.write(out);
}

pub fn write_set_encodings(out: &mut Vec<u8>, encodings: &[i32]) {
    out.extend_from_slice(&[client_type::SET_ENCODINGS, 0]);
    out.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for e in encodings {
        out.extend_from_slice(&e.to_be_bytes());
    }
}

pub fn write_framebuffer_update_request(
    out: &mut Vec<u8>,
    incremental: bool,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
) {
    out.extend_from_slice(&[client_type::FRAMEBUFFER_UPDATE_REQUEST, u8::from(incremental)]);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
}

pub fn write_key_event(out: &mut Vec<u8>, down: bool, keysym: u32) {
    out.extend_from_slice(&[client_type::KEY_EVENT, u8::from(down), 0, 0]);
    out.extend_from_slice(&keysym.to_be_bytes());
}

pub fn write_pointer_event(out: &mut Vec<u8>, buttons: u8, x: u16, y: u16) {
    out.extend_from_slice(&[client_type::POINTER_EVENT, buttons]);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
}

pub fn write_client_cut_text(out: &mut Vec<u8>, text: &str) {
    let bytes = string_to_latin1(text);
    out.extend_from_slice(&[client_type::CLIENT_CUT_TEXT, 0, 0, 0]);
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&bytes);
}

pub fn write_enable_continuous_updates(out: &mut Vec<u8>, enable: bool, x: u16, y: u16, w: u16, h: u16) {
    out.extend_from_slice(&[client_type::ENABLE_CONTINUOUS_UPDATES, u8::from(enable)]);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
}

pub fn write_client_fence(out: &mut Vec<u8>, flags: u32, payload: &[u8]) {
    out.extend_from_slice(&[client_type::CLIENT_FENCE, 0, 0, 0]);
    out.extend_from_slice(&flags.to_be_bytes());
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
}

pub fn write_qemu_key_event(out: &mut Vec<u8>, down: bool, keysym: u32, keycode: u32) {
    out.extend_from_slice(&[client_type::QEMU, 0]);
    out.extend_from_slice(&u16::from(down).to_be_bytes());
    out.extend_from_slice(&keysym.to_be_bytes());
    out.extend_from_slice(&keycode.to_be_bytes());
}

// Server message writers.

/// The head of a FramebufferUpdate; `n_rects` rectangle headers and their
/// payloads follow.
pub fn write_framebuffer_update_header(out: &mut Vec<u8>, n_rects: u16) {
    out.extend_from_slice(&[server_type::FRAMEBUFFER_UPDATE, 0]);
    out.extend_from_slice(&n_rects.to_be_bytes());
}

pub fn write_rect_header(out: &mut Vec<u8>, x: u16, y: u16, w: u16, h: u16, encoding: i32) {
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&encoding.to_be_bytes());
}

pub fn write_bell(out: &mut Vec<u8>) {
    out.push(server_type::BELL);
}

pub fn write_server_cut_text(out: &mut Vec<u8>, text: &str) {
    let bytes = string_to_latin1(text);
    out.extend_from_slice(&[server_type::SERVER_CUT_TEXT, 0, 0, 0]);
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&bytes);
}

pub fn write_end_of_continuous_updates(out: &mut Vec<u8>) {
    out.push(server_type::END_OF_CONTINUOUS_UPDATES);
}

pub fn write_server_fence(out: &mut Vec<u8>, flags: u32, payload: &[u8]) {
    out.extend_from_slice(&[server_type::SERVER_FENCE, 0, 0, 0]);
    out.extend_from_slice(&flags.to_be_bytes());
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
}

/// Why an ExtendedDesktopSize rectangle was sent: its `x` field.
pub mod resize_reason {
    pub const SERVER: u16 = 0;
    pub const THIS_CLIENT: u16 = 1;
    pub const OTHER_CLIENT: u16 = 2;
}

/// How a client's SetDesktopSize went: the rectangle's `y` field.
pub mod resize_status {
    pub const OK: u16 = 0;
    pub const PROHIBITED: u16 = 1;
    pub const OUT_OF_RESOURCES: u16 = 2;
    pub const INVALID_LAYOUT: u16 = 3;
}

/// The payload of an ExtendedDesktopSize rectangle: the screen list.
pub fn write_extended_desktop_size(out: &mut Vec<u8>, screens: &[Screen]) {
    out.extend_from_slice(&[screens.len() as u8, 0, 0, 0]);
    for s in screens {
        out.extend_from_slice(&s.id.to_be_bytes());
        out.extend_from_slice(&s.x.to_be_bytes());
        out.extend_from_slice(&s.y.to_be_bytes());
        out.extend_from_slice(&s.width.to_be_bytes());
        out.extend_from_slice(&s.height.to_be_bytes());
        out.extend_from_slice(&s.flags.to_be_bytes());
    }
}

/// The screen list of an ExtendedDesktopSize rectangle, for a client.
pub fn parse_extended_desktop_size(buf: &[u8]) -> Result<Option<(Vec<Screen>, usize)>, Error> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let n = usize::from(buf[0]);
    if n > MAX_SCREENS {
        return Err(Error::TooLong {
            what: "screen list",
            len: n,
            limit: MAX_SCREENS,
        });
    }
    let total = 4 + n * 16;
    if buf.len() < total {
        return Ok(None);
    }
    let screens = buf[4..total]
        .as_chunks::<16>()
        .0
        .iter()
        .map(|b| Screen {
            id: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            x: u16::from_be_bytes([b[4], b[5]]),
            y: u16::from_be_bytes([b[6], b[7]]),
            width: u16::from_be_bytes([b[8], b[9]]),
            height: u16::from_be_bytes([b[10], b[11]]),
            flags: u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
        })
        .collect();
    Ok(Some((screens, total)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) -> ClientMessage {
        // Every proper prefix must ask for more; the whole thing must parse
        // and use exactly its own length.
        for n in 0..bytes.len() {
            assert_eq!(parse_client(&bytes[..n]).unwrap(), None, "prefix of {n} bytes");
        }
        let (msg, used) = parse_client(bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        // Trailing bytes belong to the next message and are left alone.
        let mut with_tail = bytes.to_vec();
        with_tail.extend_from_slice(&[9, 9, 9]);
        assert_eq!(
            parse_client(&with_tail).unwrap(),
            Some((msg.clone(), bytes.len()))
        );
        msg
    }

    #[test]
    fn every_client_message_round_trips() {
        let mut out = Vec::new();

        write_set_pixel_format(&mut out, &PixelFormat::rgb565());
        assert_eq!(
            round_trip(&out),
            ClientMessage::SetPixelFormat(PixelFormat::rgb565())
        );
        out.clear();

        write_set_encodings(&mut out, &[7, 16, 0, -239, -313]);
        assert_eq!(
            round_trip(&out),
            ClientMessage::SetEncodings(vec![7, 16, 0, -239, -313])
        );
        out.clear();

        write_framebuffer_update_request(&mut out, true, 1, 2, 300, 400);
        assert_eq!(
            round_trip(&out),
            ClientMessage::FramebufferUpdateRequest {
                incremental: true,
                x: 1,
                y: 2,
                width: 300,
                height: 400
            }
        );
        out.clear();

        write_key_event(&mut out, true, 0xff0d);
        assert_eq!(
            round_trip(&out),
            ClientMessage::KeyEvent {
                down: true,
                keysym: 0xff0d
            }
        );
        out.clear();

        write_pointer_event(&mut out, 0b101, 640, 360);
        assert_eq!(
            round_trip(&out),
            ClientMessage::PointerEvent {
                buttons: 5,
                x: 640,
                y: 360
            }
        );
        out.clear();

        write_client_cut_text(&mut out, "héllo ✓");
        assert_eq!(round_trip(&out), ClientMessage::ClientCutText("héllo ?".into()));
        out.clear();

        write_enable_continuous_updates(&mut out, true, 0, 0, 1280, 720);
        assert_eq!(
            round_trip(&out),
            ClientMessage::EnableContinuousUpdates {
                enable: true,
                x: 0,
                y: 0,
                width: 1280,
                height: 720
            }
        );
        out.clear();

        write_client_fence(&mut out, FENCE_REQUEST | FENCE_SYNC_NEXT, &[1, 2, 3]);
        assert_eq!(
            round_trip(&out),
            ClientMessage::ClientFence {
                flags: FENCE_REQUEST | FENCE_SYNC_NEXT,
                payload: vec![1, 2, 3]
            }
        );
        out.clear();

        write_qemu_key_event(&mut out, false, 0x61, 0x1e);
        assert_eq!(
            round_trip(&out),
            ClientMessage::QemuKeyEvent {
                down: false,
                keysym: 0x61,
                keycode: 0x1e
            }
        );
        out.clear();
    }

    #[test]
    fn set_desktop_size_and_extended_clipboard() {
        let mut out = vec![client_type::SET_DESKTOP_SIZE, 0];
        out.extend_from_slice(&1920u16.to_be_bytes());
        out.extend_from_slice(&1080u16.to_be_bytes());
        out.extend_from_slice(&[1, 0]);
        out.extend_from_slice(&7u32.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&1920u16.to_be_bytes());
        out.extend_from_slice(&1080u16.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            round_trip(&out),
            ClientMessage::SetDesktopSize {
                width: 1920,
                height: 1080,
                screens: vec![Screen {
                    id: 7,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    flags: 0
                }],
            }
        );

        // A negative length means the extension rather than a string.
        let caps = clipboard_caps(
            clipboard::format::TEXT,
            clipboard::action::NOTIFY | clipboard::action::PROVIDE,
            &[MAX_CUT_TEXT as u32],
        );
        let mut out = Vec::new();
        write_client_clipboard(&mut out, &caps);
        assert_eq!(
            round_trip(&out),
            ClientMessage::ExtendedClipboard(ClipboardMessage::Caps {
                formats: clipboard::format::TEXT,
                actions: clipboard::action::CAPS | clipboard::action::NOTIFY | clipboard::action::PROVIDE,
                sizes: vec![MAX_CUT_TEXT as u32],
            })
        );
    }

    /// The extension's own bodies, each way round.
    #[test]
    fn extended_clipboard_bodies_round_trip() {
        use clipboard::{action, format};

        // Caps: the sizes are one per format bit, lowest first, and the
        // actions come back as the peer named them.
        let body = clipboard_caps(
            format::TEXT | format::HTML,
            action::REQUEST | action::NOTIFY | action::PROVIDE,
            &[4096, 16],
        );
        assert_eq!(
            parse_clipboard(&body).unwrap(),
            ClipboardMessage::Caps {
                formats: format::TEXT | format::HTML,
                actions: action::CAPS | action::REQUEST | action::NOTIFY | action::PROVIDE,
                sizes: vec![4096, 16],
            }
        );

        // The three that are flags and nothing else.
        for (action, expected) in [
            (action::REQUEST, ClipboardMessage::Request(format::TEXT)),
            (action::PEEK, ClipboardMessage::Peek(format::TEXT)),
            (action::NOTIFY, ClipboardMessage::Notify(format::TEXT)),
        ] {
            let body = clipboard_flags(action, format::TEXT);
            assert_eq!(body.len(), 4);
            assert_eq!(parse_clipboard(&body).unwrap(), expected);
        }

        // Provide, which is the whole point: UTF-8 through, not Latin-1.
        let body = clipboard_provide_text("héllo ✓");
        assert_eq!(
            parse_clipboard(&body).unwrap(),
            ClipboardMessage::Provide {
                text: Some("héllo ✓".into())
            }
        );
    }

    /// The same text, both ways out: the extension keeps it and the plain
    /// message cannot. This is the difference the whole extension buys.
    #[test]
    fn the_plain_message_loses_what_the_extension_keeps() {
        let mut plain = Vec::new();
        write_server_cut_text(&mut plain, "héllo ✓");
        // Latin-1 has an e-acute, in one byte, and has no tick at all.
        assert_eq!(&plain[8..], b"h\xe9llo ?");
        assert_eq!(latin1_to_string(&plain[8..]), "héllo ?");

        let body = clipboard_provide_text("héllo ✓");
        let ClipboardMessage::Provide { text } = parse_clipboard(&body).unwrap() else {
            panic!("a provide");
        };
        assert_eq!(text.as_deref(), Some("héllo ✓"));
    }

    /// A format this build does not read still has its block stepped over,
    /// or everything after it in the same provide is read at the wrong
    /// offset.
    #[test]
    fn a_format_in_front_of_the_text_is_stepped_over() {
        use std::io::Write;

        use clipboard::{action, format};

        // RTF is bit 1 and text is bit 0, so text comes first; put DIB
        // (bit 3) after it, and a reserved bit 7 after that.
        let flags = action::PROVIDE | format::TEXT | format::DIB | (1 << 7);
        let mut body = flags.to_be_bytes().to_vec();
        let mut flat = Vec::new();
        for block in [&b"hello\0"[..], &b"\x89PNG"[..], &b"whatever this is"[..]] {
            flat.extend_from_slice(&(block.len() as u32).to_be_bytes());
            flat.extend_from_slice(block);
        }
        let mut z = flate2::write::ZlibEncoder::new(&mut body, flate2::Compression::new(6));
        z.write_all(&flat).unwrap();
        z.finish().unwrap();

        assert_eq!(
            parse_clipboard(&body).unwrap(),
            ClipboardMessage::Provide {
                text: Some("hello".into())
            }
        );
    }

    /// What a peer can get wrong, answered without dropping the session
    /// where the extension allows it and refused where it does not.
    #[test]
    fn a_hostile_clipboard_body_is_refused_rather_than_believed() {
        use clipboard::{action, format};

        // Too short to hold its own flags.
        assert!(matches!(
            parse_clipboard(&[0, 0, 0]),
            Err(Error::Truncated("extended clipboard"))
        ));

        // Caps naming two formats and carrying one size.
        let mut body = (action::CAPS | format::TEXT | format::HTML)
            .to_be_bytes()
            .to_vec();
        body.extend_from_slice(&4096u32.to_be_bytes());
        assert!(matches!(
            parse_clipboard(&body),
            Err(Error::Truncated("extended clipboard caps"))
        ));

        // A provide whose stream is noise.
        let mut body = (action::PROVIDE | format::TEXT).to_be_bytes().to_vec();
        body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert!(matches!(parse_clipboard(&body), Err(Error::Clipboard(_))));

        // A provide whose length word runs past the data it inflated to.
        let mut body = (action::PROVIDE | format::TEXT).to_be_bytes().to_vec();
        let mut flat = 9_999u32.to_be_bytes().to_vec();
        flat.extend_from_slice(b"short");
        let mut z = flate2::write::ZlibEncoder::new(&mut body, flate2::Compression::new(6));
        std::io::Write::write_all(&mut z, &flat).unwrap();
        z.finish().unwrap();
        assert!(matches!(
            parse_clipboard(&body),
            Err(Error::Truncated("extended clipboard provide"))
        ));

        // An action nobody here knows is ignored, not refused: the
        // extension is meant to grow.
        let body = clipboard_flags(1 << 29, format::TEXT);
        assert_eq!(
            parse_clipboard(&body).unwrap(),
            ClipboardMessage::Unknown((1 << 29) | format::TEXT)
        );
    }

    /// The shape every writer in the wild actually sends.
    ///
    /// noVNC's Deflator and TigerVNC's ZlibOutStream both flush the stream
    /// and stop: there is no final block and no Adler-32 trailer, because
    /// the same deflater is reused for the next message. A reader that asks
    /// for the end of the stream is asking for something that is never
    /// coming, and reads it as a truncation. Found against noVNC in a
    /// browser, where the server dropped the session on every paste.
    #[test]
    fn a_provide_whose_stream_is_flushed_and_not_finished_still_reads() {
        let body = clipboard_provide_text_flushed("paste from a browser ✓");
        assert_eq!(
            parse_clipboard(&body).unwrap(),
            ClipboardMessage::Provide {
                text: Some("paste from a browser ✓".into())
            }
        );

        // And a finished one still reads, which is what this server sends.
        let body = clipboard_provide_text("paste from here ✓");
        assert_eq!(
            parse_clipboard(&body).unwrap(),
            ClipboardMessage::Provide {
                text: Some("paste from here ✓".into())
            }
        );
    }

    #[test]
    fn hostile_lengths_fail_before_any_wait() {
        let mut out = vec![client_type::CLIENT_CUT_TEXT, 0, 0, 0];
        out.extend_from_slice(&(MAX_CUT_TEXT as u32 + 1).to_be_bytes());
        assert!(matches!(
            parse_client(&out),
            Err(Error::TooLong { what: "cut text", .. })
        ));

        let mut out = vec![client_type::SET_ENCODINGS, 0];
        out.extend_from_slice(&(MAX_ENCODINGS as u16 + 1).to_be_bytes());
        assert!(matches!(
            parse_client(&out),
            Err(Error::TooLong {
                what: "encoding list",
                ..
            })
        ));

        assert_eq!(parse_client(&[200]), Err(Error::UnknownMessageType(200)));
        assert_eq!(parse_client(&[255, 9]), Err(Error::UnknownQemuSubtype(9)));
    }

    #[test]
    fn server_writers_lay_out_the_headers() {
        let mut out = Vec::new();
        write_framebuffer_update_header(&mut out, 2);
        write_rect_header(&mut out, 10, 20, 30, 40, -239);
        assert_eq!(
            out,
            [0, 0, 0, 2, 0, 10, 0, 20, 0, 30, 0, 40, 0xff, 0xff, 0xff, 0x11]
        );
        out.clear();
        write_server_fence(&mut out, FENCE_SYNC_NEXT, &[7]);
        assert_eq!(out, [248, 0, 0, 0, 0, 0, 0, 4, 1, 7]);
        out.clear();
        write_server_cut_text(&mut out, "ok");
        assert_eq!(out, [3, 0, 0, 0, 0, 0, 0, 2, b'o', b'k']);
    }
}
