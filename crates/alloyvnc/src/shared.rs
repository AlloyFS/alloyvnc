//! What the capture thread and every session share.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use alloyvnc_encode::{CursorShape, Framebuffer};
use alloyvnc_region::{Move, Rect, Region};
use alloyvnc_screen::{Clipboard, Input};

use crate::stats::{CaptureStats, SessionStats};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{broadcast, watch};

/// "Frame `seq` changed this." Every session folds these into its own
/// pending state; a session that falls behind the channel marks the whole
/// picture dirty and catches up on the next update.
#[derive(Clone, Debug)]
pub struct FrameEvent {
    pub seq: u64,
    /// When the capture thread finished writing this frame. A session
    /// subtracts it from the clock as an update goes out, which is the
    /// "capture to socket" number the plan asks for: everything the server
    /// adds between a pixel changing and the bytes leaving.
    pub at: Instant,
    pub damage: Arc<Region>,
    pub moves: Arc<Vec<Move>>,
    pub cursor: Option<Arc<CursorShape>>,
    pub resized: bool,
}

/// What the desk's clipboard holds, and a count that moves on every change.
///
/// The count is what a session watches rather than the text: somebody who
/// copies the same word twice has changed the clipboard twice, and a client
/// that asked to be told about changes should hear about both.
#[derive(Clone, Debug, Default)]
pub struct Clip {
    pub seq: u64,
    pub text: String,
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
    /// The desk's clipboard. The capture thread asks it what changed; a
    /// session pushes a client's text onto it.
    pub clipboard: Mutex<Box<dyn Clipboard>>,
    /// The last thing the desk copied, for every session to see at once.
    pub clip: watch::Sender<Clip>,
    /// Every live session's counters, for the stats endpoint. Holding a
    /// second reference rather than reaching into the session means reading
    /// them never waits on whatever that session is doing.
    pub sessions: Mutex<Vec<Arc<SessionStats>>>,
    pub capture: CaptureStats,
}

impl Shared {
    /// Sixty-four events of backlog: a session further behind than that is
    /// told to redraw, which costs less than the queue would.
    const FRAME_BACKLOG: usize = 64;

    pub fn new(
        name: impl Into<String>,
        width: u32,
        height: u32,
        input: Box<dyn Input>,
        clipboard: Box<dyn Clipboard>,
    ) -> Arc<Shared> {
        let (frames, _) = broadcast::channel(Self::FRAME_BACKLOG);
        let (clip, _) = watch::channel(Clip::default());
        Arc::new(Shared {
            name: name.into(),
            fb: RwLock::new(Framebuffer::new(width, height)),
            screens: Mutex::new(vec![Rect::new(0, 0, width as i32, height as i32)]),
            cursor: Mutex::new(None),
            frames,
            seq: AtomicU64::new(0),
            input: Mutex::new(input),
            clipboard: Mutex::new(clipboard),
            clip,
            sessions: Mutex::new(Vec::new()),
            capture: CaptureStats::default(),
        })
    }

    pub fn register(&self, stats: Arc<SessionStats>) {
        self.sessions.lock().push(stats);
    }

    pub fn unregister(&self, stats: &Arc<SessionStats>) {
        self.sessions.lock().retain(|s| !Arc::ptr_eq(s, stats));
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
}
