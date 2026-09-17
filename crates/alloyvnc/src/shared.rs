//! What the capture thread and every session share.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloyvnc_encode::Framebuffer;
use alloyvnc_region::Region;
use alloyvnc_screen::Input;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

/// "Frame `seq` changed `region`." Every session folds these into its own
/// pending damage; a session that falls behind the channel marks the whole
/// picture dirty and catches up on the next update.
#[derive(Clone, Debug)]
pub struct DamageEvent {
    pub seq: u64,
    pub region: Arc<Region>,
}

pub struct Shared {
    /// The desktop name a viewer shows in its title bar.
    pub name: String,
    /// The picture. The capture thread takes the write lock only to blit
    /// a frame's dirty rectangles; encoders take the read lock per update.
    pub fb: RwLock<Framebuffer>,
    pub damage: broadcast::Sender<DamageEvent>,
    /// Frames applied so far.
    pub seq: AtomicU64,
    pub input: Mutex<Box<dyn Input>>,
}

impl Shared {
    /// Sixty-four events of backlog: a session further behind than that is
    /// told to redraw, which costs less than the queue would.
    const DAMAGE_BACKLOG: usize = 64;

    pub fn new(name: impl Into<String>, width: u32, height: u32, input: Box<dyn Input>) -> Arc<Shared> {
        let (damage, _) = broadcast::channel(Self::DAMAGE_BACKLOG);
        Arc::new(Shared {
            name: name.into(),
            fb: RwLock::new(Framebuffer::new(width, height)),
            damage,
            seq: AtomicU64::new(0),
            input: Mutex::new(input),
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
}
