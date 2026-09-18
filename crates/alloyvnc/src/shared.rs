//! What the capture thread and every session share.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloyvnc_encode::{CursorShape, Framebuffer};
use alloyvnc_region::{Move, Rect, Region};
use alloyvnc_screen::Input;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

/// "Frame `seq` changed this." Every session folds these into its own
/// pending state; a session that falls behind the channel marks the whole
/// picture dirty and catches up on the next update.
#[derive(Clone, Debug)]
pub struct FrameEvent {
    pub seq: u64,
    pub damage: Arc<Region>,
    pub moves: Arc<Vec<Move>>,
    pub cursor: Option<Arc<CursorShape>>,
    pub resized: bool,
}

pub struct Shared {
    /// The desktop name a viewer shows in its title bar.
    pub name: String,
    /// The picture. The capture thread takes the write lock only to blit
    /// a frame's dirty rectangles; encoders take the read lock per update.
    pub fb: RwLock<Framebuffer>,
    /// The monitors making up the picture, in picture coordinates.
    pub screens: Mutex<Vec<Rect>>,
    /// The pointer as last reported, so a client that connects between two
    /// shape changes still gets one.
    pub cursor: Mutex<Option<Arc<CursorShape>>>,
    pub frames: broadcast::Sender<FrameEvent>,
    /// Frames applied so far.
    pub seq: AtomicU64,
    pub input: Mutex<Box<dyn Input>>,
}

impl Shared {
    /// Sixty-four events of backlog: a session further behind than that is
    /// told to redraw, which costs less than the queue would.
    const FRAME_BACKLOG: usize = 64;

    pub fn new(name: impl Into<String>, width: u32, height: u32, input: Box<dyn Input>) -> Arc<Shared> {
        let (frames, _) = broadcast::channel(Self::FRAME_BACKLOG);
        Arc::new(Shared {
            name: name.into(),
            fb: RwLock::new(Framebuffer::new(width, height)),
            screens: Mutex::new(vec![Rect::new(0, 0, width as i32, height as i32)]),
            cursor: Mutex::new(None),
            frames,
            seq: AtomicU64::new(0),
            input: Mutex::new(input),
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
}
