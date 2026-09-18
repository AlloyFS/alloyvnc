//! A deterministic animated screen: the backend every test and every
//! measurement runs against, on any machine, with no display attached.
//!
//! Frame `n` is a pure function of `n`, so two runs produce the same pixels
//! and the same damage. The scene has the shapes an encoder cares about: a
//! flat background with a grid, a band of high-frequency noise that
//! compresses like text and scrolls like a page (reported as a move, the
//! way a compositor would), a solid block that moves, a bar that grows, a
//! counter that changes a few pixels every frame, and a pointer whose shape
//! changes now and then.

use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Capture, CaptureError, CursorShape, Frame, Framebuffer, Move, Rect, Region};

/// Hands out frames on request rather than on a clock, so a test controls
/// exactly when the picture changes.
#[derive(Debug, Default)]
pub struct Step {
    target: Mutex<u64>,
    changed: Condvar,
}

impl Step {
    pub fn new() -> Arc<Step> {
        Arc::new(Step::default())
    }

    /// Allow `n` more frames to be drawn.
    pub fn advance(&self, n: u64) {
        let mut target = self.target.lock().unwrap_or_else(|e| e.into_inner());
        *target += n;
        self.changed.notify_all();
    }

    pub fn target(&self) -> u64 {
        *self.target.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub enum Pace {
    /// Frames on a clock, this many per second.
    Fps(u32),
    /// Frames when the [`Step`] says so.
    Manual(Arc<Step>),
}

pub struct Synth {
    width: u32,
    height: u32,
    drawn: u64,
    pace: Pace,
    next_at: Instant,
    ready: bool,
}

const BOX_SIZE: i32 = 96;
const BAR_HEIGHT: i32 = 12;
const BAR_PERIOD: u64 = 240;
const NOISE_ROWS: Range<i32> = 40..120;
/// Rows the noise band scrolls up per frame.
const SCROLL: i32 = 4;
const COUNTER_X: i32 = 8;
const COUNTER_Y: i32 = 8;
const COUNTER_CELL: i32 = 8;
const COUNTER_BITS: i32 = 12;
const CURSOR_SIZE: u32 = 12;
/// Frames between pointer shape changes.
const CURSOR_PERIOD: u64 = 30;

impl Synth {
    pub fn new(width: u32, height: u32, pace: Pace) -> Synth {
        Synth {
            width,
            height,
            drawn: 0,
            pace,
            next_at: Instant::now(),
            ready: false,
        }
    }

    pub fn frames_drawn(&self) -> u64 {
        self.drawn
    }

    /// What is under everything that moves: a grid, and a noise band that
    /// looks to an encoder like a page of text and scrolls with the frame.
    fn base_pixel(x: i32, y: i32, frame: u64) -> [u8; 4] {
        if NOISE_ROWS.contains(&y) {
            let scrolled = y as i64 + frame as i64 * i64::from(SCROLL);
            if hash(x as u32, scrolled as u32).is_multiple_of(7) {
                [235, 235, 235, 0]
            } else {
                [22, 22, 24, 0]
            }
        } else if x.rem_euclid(32) == 0 || y.rem_euclid(32) == 0 {
            [58, 58, 60, 0]
        } else {
            [28, 28, 30, 0]
        }
    }

    fn paint_base(fb: &mut Framebuffer, rect: Rect, frame: u64) {
        let r = rect.intersection(&fb.bounds());
        for y in r.y1..r.y2 {
            for x in r.x1..r.x2 {
                fb.put_pixel(x as u32, y as u32, Self::base_pixel(x, y, frame));
            }
        }
    }

    /// The block bounces below the band, so the two never overlap.
    fn box_rect(&self, frame: u64) -> Rect {
        let top = NOISE_ROWS.end;
        let w = i64::from(self.width) - i64::from(BOX_SIZE);
        let h = i64::from(self.height) - i64::from(BOX_SIZE) - i64::from(top) - i64::from(BAR_HEIGHT);
        let t = frame as i64;
        Rect::new(
            tri(t * 3, w) as i32,
            top + tri(t * 2, h) as i32,
            BOX_SIZE,
            BOX_SIZE,
        )
    }

    fn bar_len(&self, frame: u64) -> i32 {
        ((frame % BAR_PERIOD) * u64::from(self.width) / BAR_PERIOD) as i32
    }

    fn band(&self) -> Rect {
        Rect::from_corners(0, NOISE_ROWS.start, self.width as i32, NOISE_ROWS.end).intersection(&Rect::new(
            0,
            0,
            self.width as i32,
            self.height as i32,
        ))
    }

    /// An arrow whose colour turns with the frame.
    fn cursor(frame: u64) -> CursorShape {
        let n = CURSOR_SIZE as usize;
        let tint = ((frame / CURSOR_PERIOD) * 60 % 256) as u8;
        let mut rgba = vec![0u8; n * n * 4];
        for y in 0..n {
            for x in 0..n {
                let inside = x <= y && x + y < n;
                let edge = inside && (x == 0 || x == y || x + y + 1 == n);
                let px = &mut rgba[(y * n + x) * 4..][..4];
                if edge {
                    px.copy_from_slice(&[0, 0, 0, 255]);
                } else if inside {
                    px.copy_from_slice(&[255, tint, 255 - tint, 255]);
                }
            }
        }
        CursorShape::new(CURSOR_SIZE, CURSOR_SIZE, 0, 0, rgba)
    }

    fn draw(&self, fb: &mut Framebuffer, frame: u64, prev: Option<u64>) -> Frame {
        let mut damage = Vec::new();
        let mut moves = Vec::new();

        match prev {
            None => {
                let all = fb.bounds();
                Self::paint_base(fb, all, frame);
                damage.push(all);
            }
            Some(p) => {
                // The band scrolls up: a move of most of it, then fresh
                // rows at the bottom. What a compositor reports for a page
                // scroll, and what CopyRect exists for.
                let band = self.band();
                if band.height() > SCROLL {
                    let dst = Rect::from_corners(band.x1, band.y1, band.x2, band.y2 - SCROLL);
                    fb.copy_within(band.x1 as u32, (band.y1 + SCROLL) as u32, dst);
                    moves.push(Move {
                        src_x: band.x1,
                        src_y: band.y1 + SCROLL,
                        dst,
                    });
                    let fresh = Rect::from_corners(band.x1, band.y2 - SCROLL, band.x2, band.y2);
                    Self::paint_base(fb, fresh, frame);
                    damage.push(fresh);
                }
                // The block: repaint where it was.
                let old = self.box_rect(p);
                Self::paint_base(fb, old, frame);
                damage.push(old);
            }
        }
        let bx = self.box_rect(frame);
        fb.fill(bx, [(frame * 7 % 256) as u8, 120, 210, 0]);
        damage.push(bx);

        // The bar along the bottom, wrapping every BAR_PERIOD frames.
        let (y1, y2) = (self.height as i32 - BAR_HEIGHT, self.height as i32);
        let new_len = self.bar_len(frame);
        let old_len = prev.map_or(0, |p| self.bar_len(p));
        if new_len < old_len {
            let gone = Rect::from_corners(0, y1, old_len, y2);
            Self::paint_base(fb, gone, frame);
            damage.push(gone);
        }
        fb.fill(Rect::from_corners(0, y1, new_len, y2), [90, 200, 60, 0]);
        if new_len > old_len {
            damage.push(Rect::from_corners(old_len, y1, new_len, y2));
        }

        // The frame counter, one cell per bit.
        for bit in 0..COUNTER_BITS {
            let on = (frame >> bit) & 1 == 1;
            let cell = Rect::new(
                COUNTER_X + bit * COUNTER_CELL,
                COUNTER_Y,
                COUNTER_CELL - 1,
                COUNTER_CELL - 1,
            );
            fb.fill(cell, if on { [250, 250, 250, 0] } else { [40, 40, 40, 0] });
        }
        damage.push(Rect::new(
            COUNTER_X,
            COUNTER_Y,
            COUNTER_BITS * COUNTER_CELL,
            COUNTER_CELL,
        ));

        // Everything drawn was clipped to the picture; the damage must be too.
        let bounds = fb.bounds();
        let cursor =
            (prev.is_none() || frame.is_multiple_of(CURSOR_PERIOD)).then(|| Arc::new(Self::cursor(frame)));
        Frame {
            damage: Region::from_rects(damage.into_iter().map(|r| r.intersection(&bounds))),
            moves,
            cursor,
            resized: false,
        }
    }
}

/// A triangle wave over `0..=range`, so the block bounces instead of wrapping.
fn tri(t: i64, range: i64) -> i64 {
    if range <= 0 {
        return 0;
    }
    let p = t.rem_euclid(range * 2);
    if p < range { p } else { range * 2 - p }
}

fn hash(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h
}

impl Capture for Synth {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, CaptureError> {
        if self.ready {
            return Ok(true);
        }
        match &self.pace {
            Pace::Fps(_) => {
                let now = Instant::now();
                if self.next_at > now {
                    std::thread::sleep((self.next_at - now).min(timeout));
                }
                self.ready = Instant::now() >= self.next_at;
                Ok(self.ready)
            }
            Pace::Manual(step) => {
                let deadline = Instant::now() + timeout;
                let mut target = step.target.lock().unwrap_or_else(|e| e.into_inner());
                while *target <= self.drawn {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(false);
                    }
                    target = step
                        .changed
                        .wait_timeout(target, deadline - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                self.ready = true;
                Ok(true)
            }
        }
    }

    fn apply(&mut self, fb: &mut Framebuffer) -> Result<Frame, CaptureError> {
        let mut prev = (self.drawn > 0).then_some(self.drawn);
        let mut resized = false;
        if fb.width() != self.width || fb.height() != self.height {
            fb.resize(self.width, self.height);
            prev = None;
            resized = true;
        }
        self.ready = false;
        let frame = self.drawn + 1;
        let mut out = self.draw(fb, frame, prev);
        out.resized = resized;
        self.drawn = frame;
        if let Pace::Fps(fps) = self.pace {
            self.next_at += Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
            let now = Instant::now();
            if self.next_at < now {
                self.next_at = now;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(frames: u64) -> (Synth, Framebuffer, Arc<Step>) {
        let step = Step::new();
        let mut s = Synth::new(256, 200, Pace::Manual(step.clone()));
        let mut fb = Framebuffer::new(256, 200);
        step.advance(frames);
        for _ in 0..frames {
            assert!(s.wait(Duration::from_millis(10)).unwrap());
            s.apply(&mut fb).unwrap();
        }
        (s, fb, step)
    }

    #[test]
    fn deterministic() {
        let (_, a, _) = drawn(37);
        let (_, b, _) = drawn(37);
        assert_eq!(a, b);
        let (_, c, _) = drawn(38);
        assert_ne!(a, c);
    }

    #[test]
    fn damage_and_moves_cover_every_changed_pixel() {
        let (mut s, mut fb, step) = drawn(5);
        let mut cursors = 0;
        for _ in 0..300 {
            let before = fb.clone();
            step.advance(1);
            assert!(s.wait(Duration::from_millis(10)).unwrap());
            let frame = s.apply(&mut fb).unwrap();
            assert!(!frame.damage.is_empty());
            assert_eq!(frame.moves.len(), 1, "the band scrolls every frame");
            cursors += usize::from(frame.cursor.is_some());
            // Replaying the moves then the damage onto the previous picture
            // must give the new picture: the CopyRect contract.
            let mut replay = before.clone();
            for m in &frame.moves {
                replay.copy_within(m.src_x as u32, m.src_y as u32, m.dst);
            }
            for r in frame.damage.rects() {
                for y in r.y1..r.y2 {
                    let row = fb.row_span(y as u32, r.x1 as u32, r.width() as u32).to_vec();
                    replay
                        .row_span_mut(y as u32, r.x1 as u32, r.width() as u32)
                        .copy_from_slice(&row);
                }
            }
            assert_eq!(replay, fb, "moves plus damage do not reproduce the frame");
            assert!(
                frame.damage.area() < fb.bounds().area(),
                "no frame after the first repaints everything"
            );
        }
        assert_eq!(cursors, 10, "a new pointer shape every {CURSOR_PERIOD} frames");
    }

    #[test]
    fn manual_pace_waits_for_the_step() {
        let step = Step::new();
        let mut s = Synth::new(128, 128, Pace::Manual(step.clone()));
        assert!(!s.wait(Duration::from_millis(5)).unwrap());
        step.advance(1);
        assert!(s.wait(Duration::from_millis(5)).unwrap());
        let mut fb = Framebuffer::new(1, 1);
        let frame = s.apply(&mut fb).unwrap();
        assert_eq!((fb.width(), fb.height()), (128, 128), "resized to the screen");
        assert!(frame.resized);
        assert_eq!(frame.damage.bounds(), fb.bounds(), "first frame is all damage");
        assert!(frame.cursor.is_some(), "first frame carries the pointer");
        assert!(!s.wait(Duration::from_millis(5)).unwrap());
    }
}
