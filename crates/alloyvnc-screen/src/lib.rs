//! What a server captures from and types into.
//!
//! A [`Capture`] produces frames into a [`Framebuffer`] and says what
//! changed; an [`Input`] takes the client's keys and pointer. The synthetic
//! screen in [`synth`] implements both halves with no display attached, so
//! the protocol and the encoders can be tested on any machine. The real
//! backends (DXGI on Windows, X11 on Linux) live in their own crates and
//! carry the unsafe.

#![forbid(unsafe_code)]

pub mod synth;

use std::time::Duration;

pub use alloyvnc_encode::Framebuffer;
pub use alloyvnc_region::{Rect, Region};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The source went away (a mode change, a locked desktop); the backend
    /// has to be rebuilt.
    #[error("capture lost: {0}")]
    Lost(String),
    #[error("capture failed: {0}")]
    Failed(String),
}

/// A source of frames. Two steps, so the framebuffer lock is held only for
/// the blit: [`wait`](Capture::wait) blocks in the OS until a frame is ready,
/// [`apply`](Capture::apply) writes it and reports the damage.
pub trait Capture: Send {
    /// The screen's size in pixels.
    fn size(&self) -> (u32, u32);

    /// Block until a frame is ready or `timeout` passes. `Ok(true)` means
    /// [`apply`](Capture::apply) has something to write.
    fn wait(&mut self, timeout: Duration) -> Result<bool, CaptureError>;

    /// Write the ready frame into `fb`, resizing it if the screen changed,
    /// and return the region that differs from before.
    fn apply(&mut self, fb: &mut Framebuffer) -> Result<Region, CaptureError>;
}

/// Where a client's keys and pointer go.
pub trait Input: Send {
    /// An X11 keysym, pressed or released.
    fn key(&mut self, keysym: u32, down: bool);

    /// The pointer at `(x, y)` with the RFB button mask (bit 0 left, 1
    /// middle, 2 right, 3 and 4 the wheel).
    fn pointer(&mut self, x: u16, y: u16, buttons: u8);
}

/// Swallows input, for a screen nothing can type into.
pub struct NullInput;

impl Input for NullInput {
    fn key(&mut self, _keysym: u32, _down: bool) {}
    fn pointer(&mut self, _x: u16, _y: u16, _buttons: u8) {}
}
