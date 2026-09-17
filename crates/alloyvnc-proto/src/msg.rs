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
    /// An Extended Clipboard message (negative length in ClientCutText), raw.
    ExtendedClipboard(Vec<u8>),
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
                .chunks_exact(4)
                .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
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
                ClientMessage::ExtendedClipboard(raw.to_vec())
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

        let mut out = vec![client_type::CLIENT_CUT_TEXT, 0, 0, 0];
        out.extend_from_slice(&(-3i32).to_be_bytes());
        out.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert_eq!(
            round_trip(&out),
            ClientMessage::ExtendedClipboard(vec![0xaa, 0xbb, 0xcc])
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
