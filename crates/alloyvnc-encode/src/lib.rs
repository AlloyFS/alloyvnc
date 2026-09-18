//! The picture and the encoders that turn a rectangle of it into bytes in a
//! client's pixel format.
//!
//! Everything here is pure: a [`Framebuffer`] in, bytes out, no sockets and
//! no clock, so an encoder can be benchmarked on fixed input and its output
//! compared byte for byte across changes.

#![forbid(unsafe_code)]

pub mod convert;
pub mod copyrect;
pub mod cursor;
pub mod raw;

mod framebuffer;

pub use alloyvnc_proto::PixelFormat;
pub use alloyvnc_region::Rect;
pub use cursor::CursorShape;
pub use framebuffer::Framebuffer;
