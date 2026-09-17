//! The RFB wire format (RFC 6143), as bytes in and bytes out.
//!
//! Nothing in this crate does I/O. The server, the test client and any
//! recorder or proxy drive the same parsers and writers over whatever
//! transport they have, which is what keeps the protocol fuzzable and
//! testable on a machine with no display.
//!
//! Everything on the wire is big-endian, as the RFC says. Pixel data inside
//! a rectangle is the one exception: it is laid out by the client's
//! [`PixelFormat`], which carries its own endianness flag.

#![forbid(unsafe_code)]

pub mod auth;
pub mod encoding;
pub mod handshake;
pub mod msg;
pub mod pixel_format;

mod error;

pub use error::Error;
pub use pixel_format::PixelFormat;
