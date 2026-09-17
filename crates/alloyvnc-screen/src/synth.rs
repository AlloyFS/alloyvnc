//! A deterministic animated screen: the backend every test and every
//! measurement runs against, on any machine, with no display attached.
//!
//! Frame `n` is a pure function of `n`, so two runs produce the same pixels
//! and the same damage. The scene has the shapes an encoder cares about: a
//! flat background with a grid, a band of high-frequency noise that
//! compresses like text, a solid block that moves, a bar that grows, and a
//! counter that changes a few pixels every frame.

use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Capture, CaptureError, Framebuffer, Rect, Region};

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
const NOISE_ROWS: Range<u32> = 40..120;
const COUNTER_X: i32 = 8;
const COUNTER_Y: i32 = 8;
const COUNTER_CELL: i32 = 8;
const COUNTER_BITS: i32 = 12;

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
    /// looks to an encoder like a page of text.
    fn base_pixel(x: u32, y: u32) -> [u8; 4] {
        if NOISE_ROWS.contains(&y) {
            if hash(x, y).is_multiple_of(7) {
                [235, 235, 235, 0]
            } else {
                [22, 22, 24, 0]
            }
        } else if x.is_multiple_of(32) || y.is_multiple_of(32) {
            [58, 58, 60, 0]
        } else {
            [28, 28, 30, 0]
        }
    }

    fn paint_base(fb: &mut Framebuffer, rect: Rect) {
        let r = rect.intersection(&fb.bounds());
        for y in r.y1..r.y2 {
            for x in r.x1..r.x2 {
                fb.put_pixel(x as u32, y as u32, Self::base_pixel(x as u32, y as u32));
            }
        }
    }

    fn box_rect(&self, frame: u64) -> Rect {
        let w = i64::from(self.width) - i64::from(BOX_SIZE);
        let h = i64::from(self.height) - i64::from(BOX_SIZE);
        let t = frame as i64;
        Rect::new(tri(t * 3, w) as i32, tri(t * 2, h) as i32, BOX_SIZE, BOX_SIZE)
    }

    fn bar_len(&self, frame: u64) -> i32 {
        ((frame % BAR_PERIOD) * u64::from(self.width) / BAR_PERIOD) as i32
    }

    fn draw(&self, fb: &mut Framebuffer, frame: u64, prev: Option<u64>) -> Region {
        let mut damage = Vec::new();

        // The moving block: repaint where it was, draw where it is.
        match prev {
            None => {
                let all = fb.bounds();
                Self::paint_base(fb, all);
                damage.push(all);
            }
            Some(p) => {
                let old = self.box_rect(p);
                Self::paint_base(fb, old);
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
            Self::paint_base(fb, gone);
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
        Region::from_rects(damage.into_iter().map(|r| r.intersection(&bounds)))
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

    fn apply(&mut self, fb: &mut Framebuffer) -> Result<Region, CaptureError> {
        let mut prev = (self.drawn > 0).then_some(self.drawn);
        if fb.width() != self.width || fb.height() != self.height {
            fb.resize(self.width, self.height);
            prev = None;
        }
        self.ready = false;
        let frame = self.drawn + 1;
        let damage = self.draw(fb, frame, prev);
        self.drawn = frame;
        if let Pace::Fps(fps) = self.pace {
            self.next_at += Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
            let now = Instant::now();
            if self.next_at < now {
                self.next_at = now;
            }
        }
        Ok(damage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(frames: u64) -> (Synth, Framebuffer, Arc<Step>) {
        let step = Step::new();
        let mut s = Synth::new(256, 160, Pace::Manual(step.clone()));
        let mut fb = Framebuffer::new(256, 160);
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
    fn damage_covers_every_changed_pixel() {
        let (mut s, mut fb, step) = drawn(5);
        for _ in 0..300 {
            let before = fb.clone();
            step.advance(1);
            assert!(s.wait(Duration::from_millis(10)).unwrap());
            let damage = s.apply(&mut fb).unwrap();
            assert!(!damage.is_empty());
            for y in 0..fb.height() {
                for x in 0..fb.width() {
                    if fb.pixel(x, y) != before.pixel(x, y) {
                        assert!(
                            damage.contains_point(x as i32, y as i32),
                            "({x}, {y}) changed outside the damage"
                        );
                    }
                }
            }
            assert!(
                damage.area() < fb.bounds().area(),
                "no frame after the first repaints everything"
            );
        }
    }

    #[test]
    fn manual_pace_waits_for_the_step() {
        let step = Step::new();
        let mut s = Synth::new(128, 128, Pace::Manual(step.clone()));
        assert!(!s.wait(Duration::from_millis(5)).unwrap());
        step.advance(1);
        assert!(s.wait(Duration::from_millis(5)).unwrap());
        let mut fb = Framebuffer::new(1, 1);
        let damage = s.apply(&mut fb).unwrap();
        assert_eq!((fb.width(), fb.height()), (128, 128), "resized to the screen");
        assert_eq!(damage.bounds(), fb.bounds(), "first frame is all damage");
        assert!(!s.wait(Duration::from_millis(5)).unwrap());
    }
}
