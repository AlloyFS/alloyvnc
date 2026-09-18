//! The capture thread.
//!
//! A plain thread rather than a task: a capture backend blocks inside the
//! OS (DXGI's AcquireNextFrame, X11's event wait), and a blocked tokio
//! worker would stall every socket the runtime owns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use alloyvnc_screen::Capture;

use crate::shared::{FrameEvent, Shared};

/// How long one wait blocks before the stop flag is checked again.
const WAIT_SLICE: Duration = Duration::from_millis(100);
/// The pause after a capture error before trying again.
const RETRY_AFTER: Duration = Duration::from_millis(500);

pub fn spawn(shared: Arc<Shared>, mut capture: Box<dyn Capture>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            *shared.screens.lock() = capture.screens();
            while !stop.load(Ordering::Relaxed) {
                match capture.wait(WAIT_SLICE) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "capture wait failed; retrying");
                        std::thread::sleep(RETRY_AFTER);
                        continue;
                    }
                }
                let applied = {
                    let mut fb = shared.fb.write();
                    capture.apply(&mut fb)
                };
                match applied {
                    Ok(frame) if frame.is_empty() => {}
                    Ok(frame) => {
                        if frame.resized {
                            let screens = capture.screens();
                            tracing::info!(?screens, "picture resized");
                            *shared.screens.lock() = screens;
                        }
                        if let Some(shape) = &frame.cursor {
                            *shared.cursor.lock() = Some(shape.clone());
                        }
                        let seq = shared.seq.fetch_add(1, Ordering::AcqRel) + 1;
                        // No receivers is not an error: nobody is connected.
                        let _ = shared.frames.send(FrameEvent {
                            seq,
                            damage: Arc::new(frame.damage),
                            moves: Arc::new(frame.moves),
                            cursor: frame.cursor,
                            resized: frame.resized,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "capture apply failed; retrying");
                        std::thread::sleep(RETRY_AFTER);
                    }
                }
            }
            tracing::info!("capture thread stopped");
        })
        .expect("spawn the capture thread")
}
