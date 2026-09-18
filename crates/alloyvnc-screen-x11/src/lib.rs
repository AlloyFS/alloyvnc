//! Linux: the X11 desktop through XDamage and MIT-SHM, keys and the
//! pointer through XTEST.
//!
//! X11 has no compositor handing over a finished frame the way DXGI does,
//! so the backend assembles the same thing out of four extensions. XDamage
//! tells the server to report which rectangles of the root window were drawn
//! into. MIT-SHM lets the pixels be fetched into memory the X server and this
//! process both map, so a screenful is a copy rather than eight megabytes
//! down a socket. XFixes holds the pointer image and the region algebra
//! damage answers in. RandR reports a resolution change.
//!
//! What X11 has no answer for is moves: the protocol reports a scroll as
//! damage over the scrolled area, never as a block that shifted, so
//! [`Frame::moves`] is always empty here and CopyRect has to come from the
//! compare pass.
//!
//! Everything that talks to X is gated on Linux. What is left, the keyboard
//! mapping and the pointer conversion, is arithmetic over plain buffers and
//! is compiled and tested on every platform.
//!
//! [`Frame::moves`]: alloyvnc_screen::Frame::moves

pub mod cursor;
pub mod keymap;

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod input;

#[cfg(target_os = "linux")]
pub use capture::X11Capture;
#[cfg(target_os = "linux")]
pub use input::X11Input;
