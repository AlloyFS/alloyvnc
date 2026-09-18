/// What the wire can get wrong.
///
/// A parser answers `Ok(None)` for "more bytes needed" and one of these for
/// "these bytes are wrong", so a session can tell a slow client from a broken
/// one and only drop the second.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("malformed protocol version string")]
    BadVersion,
    #[error("protocol version {0}.{1} is not supported")]
    UnsupportedVersion(u16, u16),
    #[error("unknown client message type {0}")]
    UnknownMessageType(u8),
    #[error("unknown QEMU client message subtype {0}")]
    UnknownQemuSubtype(u8),
    #[error("pixel format not supported: {0}")]
    UnsupportedPixelFormat(&'static str),
    #[error("{what} too long: {len} bytes, limit {limit}")]
    TooLong {
        what: &'static str,
        len: usize,
        limit: usize,
    },
    #[error("{0} is truncated")]
    Truncated(&'static str),
    #[error("extended clipboard: {0}")]
    Clipboard(&'static str),
}
