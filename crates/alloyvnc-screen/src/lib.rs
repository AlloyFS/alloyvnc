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

use std::sync::Arc;
use std::time::Duration;

pub use alloyvnc_encode::{CursorShape, Framebuffer};
pub use alloyvnc_region::{Move, Rect, Region};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The source went away (a mode change, a locked desktop); the backend
    /// rebuilds itself on the next wait.
    #[error("capture lost: {0}")]
    Lost(String),
    #[error("capture failed: {0}")]
    Failed(String),
}

/// What one applied frame changed.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Pixels that differ from the previous frame, in picture coordinates.
    /// Includes the destinations of `moves`: the picture is complete without
    /// applying them, they are hints for CopyRect.
    pub damage: Region,
    /// Blocks the compositor reported as moved, in the order they happened.
    pub moves: Vec<Move>,
    /// A new pointer shape, or a hidden one; `None` when it did not change.
    pub cursor: Option<Arc<CursorShape>>,
    /// The picture changed size; the framebuffer was reallocated.
    pub resized: bool,
}

impl Frame {
    pub fn is_empty(&self) -> bool {
        self.damage.is_empty() && self.moves.is_empty() && self.cursor.is_none() && !self.resized
    }
}

/// A source of frames. Two steps, so the framebuffer lock is held only for
/// the blit: [`wait`](Capture::wait) blocks in the OS until a frame is ready,
/// [`apply`](Capture::apply) writes it and reports what changed.
pub trait Capture: Send {
    /// The picture's size in pixels.
    fn size(&self) -> (u32, u32);

    /// The monitors making up the picture, in picture coordinates.
    fn screens(&self) -> Vec<Rect> {
        let (w, h) = self.size();
        vec![Rect::new(0, 0, w as i32, h as i32)]
    }

    /// Block until a frame is ready or `timeout` passes. `Ok(true)` means
    /// [`apply`](Capture::apply) has something to write.
    fn wait(&mut self, timeout: Duration) -> Result<bool, CaptureError>;

    /// Write the ready frame into `fb`, resizing it if the screen changed,
    /// and report what changed.
    fn apply(&mut self, fb: &mut Framebuffer) -> Result<Frame, CaptureError>;
}

/// Where a client's keys and pointer go.
pub trait Input: Send {
    /// An X11 keysym, pressed or released.
    fn key(&mut self, keysym: u32, down: bool);

    /// The pointer at `(x, y)` in picture coordinates with the RFB button
    /// mask (bit 0 left, 1 middle, 2 right, 3 and 4 the wheel, 5 and 6 the
    /// horizontal wheel).
    fn pointer(&mut self, x: u16, y: u16, buttons: u8);
}

/// Swallows input, for a screen nothing can type into.
pub struct NullInput;

impl Input for NullInput {
    fn key(&mut self, _keysym: u32, _down: bool) {}
    fn pointer(&mut self, _x: u16, _y: u16, _buttons: u8) {}
}
