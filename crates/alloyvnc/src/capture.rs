//! The capture thread.
//!
//! A plain thread rather than a task: a capture backend blocks inside the
//! OS (DXGI's AcquireNextFrame, X11's event wait), and a blocked tokio
//! worker would stall every socket the runtime owns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alloyvnc_encode::Tightener;
use alloyvnc_screen::Capture;

use crate::shared::{FrameEvent, Shared};

/// How long one wait blocks before the stop flag is checked again.
const WAIT_SLICE: Duration = Duration::from_millis(100);
/// The pause after a capture error before trying again.
const RETRY_AFTER: Duration = Duration::from_millis(500);
/// How often the compare pass's running totals go to the log.
const TOTALS_EVERY: Duration = Duration::from_secs(5);

/// What the compare pass did over the last few seconds. Areas in pixels.
#[derive(Default)]
struct Totals {
    frames: u64,
    empty: u64,
    reported: i64,
    tightened: i64,
    moves: u64,
    micros: u64,
}

pub fn spawn(shared: Arc<Shared>, mut capture: Box<dyn Capture>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            *shared.screens.lock() = capture.screens();
            let mut tightener = Tightener::new();
            let mut totals = Totals::default();
            let mut totals_at = Instant::now();
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
                // The numbers come out of the locked section and are
                // logged outside it: a session waiting on the framebuffer
                // should not be held up by formatting a line.
                let mut pass = None;
                let applied = {
                    let mut fb = shared.fb.write();
                    match capture.apply(&mut fb) {
                        Ok(mut frame) => {
                            // Under the same lock as the blit: the pass
                            // reads the pixels that were just written, and a
                            // session taking the read lock in between would
                            // see damage nothing had narrowed yet. A resized
                            // frame needs no separate reset, since the
                            // framebuffer it is handed is a new size and
                            // that is what the pass keys on.
                            let reported = frame.damage.area();
                            let started = Instant::now();
                            let tight = tightener.tighten(&fb, &frame.damage, &frame.moves);
                            let micros = started.elapsed().as_micros() as u64;
                            pass = Some((reported, tight.damage.area(), tight.moves.len(), micros));
                            frame.damage = tight.damage;
                            frame.moves = tight.moves;
                            Ok(frame)
                        }
                        Err(e) => Err(e),
                    }
                };
                if let Some((reported, tightened, moves, micros)) = pass {
                    tracing::trace!(reported, tightened, moves, micros, "compared");
                    totals.frames += 1;
                    totals.reported += reported;
                    totals.tightened += tightened;
                    totals.moves += moves as u64;
                    totals.micros += micros;
                    if totals_at.elapsed() >= TOTALS_EVERY {
                        tracing::debug!(
                            frames = totals.frames,
                            empty = totals.empty,
                            reported = totals.reported,
                            tightened = totals.tightened,
                            moves = totals.moves,
                            micros = totals.micros,
                            "compare pass"
                        );
                        totals = Totals::default();
                        totals_at = Instant::now();
                    }
                }
                match applied {
                    // Nothing of the picture actually differs: the backend
                    // said otherwise, and the pass is why no client hears
                    // about it.
                    Ok(frame) if frame.is_empty() => totals.empty += 1,
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
