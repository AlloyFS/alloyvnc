//! The opening exchange: protocol version, security type, the result, and
//! the ClientInit/ServerInit pair (RFC 6143 sections 7.1 to 7.3).
//!
//! Three flows exist because the protocol grew. In 3.3 the server picks the
//! security type alone; in 3.7 the client picks from a list; 3.8 adds a
//! result for the None type and a reason string on failure.

use crate::{Error, PixelFormat};

pub const VERSION_LEN: usize = 12;

/// Security types, section 7.1.2 and the registry.
pub mod security {
    pub const INVALID: u8 = 0;
    pub const NONE: u8 = 1;
    pub const VNC_AUTH: u8 = 2;
    pub const TIGHT: u8 = 16;
    pub const VENCRYPT: u8 = 19;
}

/// Which variant of the handshake a client's version calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    V33,
    V37,
    V38,
}

impl Flow {
    /// 3.3 to 3.6 all behave as 3.3 (the RFC says so for 3.5; 3.4 and 3.6
    /// were RealVNC's and never differed in the handshake). Anything past
    /// 3.8 is treated as 3.8, which is what every server does.
    pub fn for_version(major: u16, minor: u16) -> Result<Flow, Error> {
        if major != 3 {
            return Err(Error::UnsupportedVersion(major, minor));
        }
        match minor {
            0..=2 => Err(Error::UnsupportedVersion(major, minor)),
            3..=6 => Ok(Flow::V33),
            7 => Ok(Flow::V37),
            _ => Ok(Flow::V38),
        }
    }

    pub const fn version_bytes(self) -> [u8; VERSION_LEN] {
        match self {
            Flow::V33 => *b"RFB 003.003\n",
            Flow::V37 => *b"RFB 003.007\n",
            Flow::V38 => *b"RFB 003.008\n",
        }
    }
}

/// `RFB xxx.yyy\n`.
pub fn parse_version(buf: &[u8]) -> Result<(u16, u16), Error> {
    if buf.len() != VERSION_LEN || &buf[..4] != b"RFB " || buf[7] != b'.' || buf[11] != b'\n' {
        return Err(Error::BadVersion);
    }
    let num = |s: &[u8]| -> Result<u16, Error> {
        if !s.iter().all(u8::is_ascii_digit) {
            return Err(Error::BadVersion);
        }
        Ok(s.iter().fold(0u16, |acc, &d| acc * 10 + u16::from(d - b'0')))
    };
    Ok((num(&buf[4..7])?, num(&buf[8..11])?))
}

/// 3.7 and 3.8: the list the client picks from.
pub fn write_security_types(out: &mut Vec<u8>, types: &[u8]) {
    out.push(types.len() as u8);
    out.extend_from_slice(types);
}

/// 3.3: the server's one choice, as a u32.
pub fn write_security_type_v33(out: &mut Vec<u8>, security_type: u8) {
    out.extend_from_slice(&u32::from(security_type).to_be_bytes());
}

/// The server has no security type to offer, with the reason why.
pub fn write_security_types_failure(out: &mut Vec<u8>, flow: Flow, reason: &str) {
    match flow {
        Flow::V33 => out.extend_from_slice(&0u32.to_be_bytes()),
        Flow::V37 | Flow::V38 => out.push(0),
    }
    write_reason(out, reason);
}

/// SecurityResult. Only 3.8 carries a reason on failure.
pub fn write_security_result(out: &mut Vec<u8>, flow: Flow, ok: bool, reason: &str) {
    out.extend_from_slice(&u32::from(!ok).to_be_bytes());
    if !ok && flow == Flow::V38 {
        write_reason(out, reason);
    }
}

fn write_reason(out: &mut Vec<u8>, reason: &str) {
    out.extend_from_slice(&(reason.len() as u32).to_be_bytes());
    out.extend_from_slice(reason.as_bytes());
}

/// ServerInit: the framebuffer's size, the server's native pixel format and
/// the desktop name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub pixel_format: PixelFormat,
    pub name: String,
}

impl ServerInit {
    pub const MAX_NAME: usize = 4096;

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        self.pixel_format.write(out);
        let name = &self.name.as_bytes()[..self.name.len().min(Self::MAX_NAME)];
        out.extend_from_slice(&(name.len() as u32).to_be_bytes());
        out.extend_from_slice(name);
    }

    /// `Ok(None)` while the message is still arriving.
    pub fn parse(buf: &[u8]) -> Result<Option<(ServerInit, usize)>, Error> {
        const HEAD: usize = 2 + 2 + PixelFormat::WIRE_LEN + 4;
        if buf.len() < HEAD {
            return Ok(None);
        }
        let width = u16::from_be_bytes([buf[0], buf[1]]);
        let height = u16::from_be_bytes([buf[2], buf[3]]);
        let pixel_format = PixelFormat::parse(&buf[4..])?;
        let len = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]) as usize;
        if len > Self::MAX_NAME {
            return Err(Error::TooLong {
                what: "desktop name",
                len,
                limit: Self::MAX_NAME,
            });
        }
        if buf.len() < HEAD + len {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&buf[HEAD..HEAD + len]).into_owned();
        Ok(Some((
            ServerInit {
                width,
                height,
                pixel_format,
                name,
            },
            HEAD + len,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Ok((3, 8)));
        assert_eq!(parse_version(b"RFB 003.003\n"), Ok((3, 3)));
        assert_eq!(parse_version(b"RFB 004.001\n"), Ok((4, 1)));
        assert_eq!(parse_version(b"RFB 003.008"), Err(Error::BadVersion));
        assert_eq!(parse_version(b"RFB 00x.008\n"), Err(Error::BadVersion));
        assert_eq!(Flow::for_version(3, 3), Ok(Flow::V33));
        assert_eq!(Flow::for_version(3, 5), Ok(Flow::V33));
        assert_eq!(Flow::for_version(3, 7), Ok(Flow::V37));
        assert_eq!(Flow::for_version(3, 8), Ok(Flow::V38));
        assert_eq!(Flow::for_version(3, 889), Ok(Flow::V38));
        assert_eq!(Flow::for_version(4, 1), Err(Error::UnsupportedVersion(4, 1)));
        assert_eq!(parse_version(&Flow::V38.version_bytes()), Ok((3, 8)));
    }

    #[test]
    fn security_result_reason_only_on_38_failure() {
        let mut out = Vec::new();
        write_security_result(&mut out, Flow::V38, true, "ignored");
        assert_eq!(out, [0, 0, 0, 0]);
        out.clear();
        write_security_result(&mut out, Flow::V37, false, "no");
        assert_eq!(out, [0, 0, 0, 1]);
        out.clear();
        write_security_result(&mut out, Flow::V38, false, "no");
        assert_eq!(out, [0, 0, 0, 1, 0, 0, 0, 2, b'n', b'o']);
    }

    #[test]
    fn server_init_round_trips() {
        let init = ServerInit {
            width: 1280,
            height: 720,
            pixel_format: PixelFormat::bgrx32(),
            name: "desk".into(),
        };
        let mut out = Vec::new();
        init.write(&mut out);
        assert_eq!(ServerInit::parse(&out[..10]).unwrap(), None);
        assert_eq!(ServerInit::parse(&out[..out.len() - 1]).unwrap(), None);
        assert_eq!(ServerInit::parse(&out).unwrap(), Some((init, out.len())));
    }
}
