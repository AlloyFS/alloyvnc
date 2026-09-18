//! What actually changed, and whether it scrolled.
//!
//! Neither backend is precise about damage. DXGI reports a whole window when
//! one pixel of it was repainted; X11 reports the region the server banded
//! its drawing into, which for one root fill is hundreds of rectangles that
//! together cover the screen. And neither reports a block that moved, so a
//! page scroll arrives as a screenful of pixels rather than as the copy it
//! is. This pass sits between the backend and the sessions and fixes both:
//! it narrows the damage to the pixels that differ, and it recognises a
//! vertical shift inside that damage so CopyRect exists on both platforms.
//!
//! **Why hashes and not the pixels.** Keeping the previous frame and
//! comparing it costs a second framebuffer, eight megabytes at 1080p, and a
//! read of both. A hash of every 64-pixel run of every row costs eight bytes
//! per 256, a thirty-second of that, and one pass over only the pixels the
//! backend already said were suspect. xxh3 runs at a few cycles a byte, so
//! hashing a rectangle is cheaper than reading the old pixels next to it
//! would be. What it buys beyond memory is the scroll detector: two rows are
//! compared in one integer comparison instead of eight kilobytes, which is
//! what makes trying every candidate shift affordable.
//!
//! **What a hash cell is.** One row of one 64-pixel column: 256 bytes, since
//! the framebuffer is four bytes a pixel. The rightmost column is narrower
//! when the width is not a multiple of 64. Damage is reported in whole
//! cells, so a one-pixel change goes out as a 64-pixel rectangle; that is
//! the trade for not keeping the old pixels, and it is a good one at any
//! rectangle a real client redraws.

use std::collections::HashMap;
use std::ops::Range;

use alloyvnc_region::{Move, Rect, Region};
use xxhash_rust::xxh3::xxh3_64;

use crate::Framebuffer;

/// Pixels across one hash cell.
pub const SEGMENT: i32 = 64;

/// Rows a detected scroll has to cover before it is worth a CopyRect. Below
/// this the rectangle header costs more than the pixels it saves.
const MIN_RUN: i32 = 8;

/// Cells a candidate shift has to explain before it is worth checking.
const MIN_VOTES: usize = 8;

/// Changed rows sampled to find the shift. A scroll moves all of itself
/// by one amount, so a few rows of it name that amount as well as all of
/// them would, and the cost of the search is this times the box.
const PROBES: usize = 16;

/// No hash recorded yet: every cell reads as this until it is first seen, so
/// the first frame after a resize reports all of itself. A real hash landing
/// on this value would leave one cell stale for one frame, which is a one in
/// eighteen quintillion event and the same bet every content hash makes.
const UNKNOWN: u64 = u64::MAX;

/// What one frame really changed.
#[derive(Clone, Debug, Default)]
pub struct Tightened {
    pub damage: Region,
    pub moves: Vec<Move>,
}

pub struct Tightener {
    size: (u32, u32),
    /// Hash cells across one row.
    cols: usize,
    /// One hash per cell, row-major, as the picture was when this pass last
    /// looked at it.
    hashes: Vec<u64>,
    /// A bit per cell: what this frame changed.
    changed: Vec<u64>,
    /// The cells of the damage's bounding box as they were before this
    /// frame, which is what the detector compares against.
    before: Vec<u64>,
}

impl Default for Tightener {
    fn default() -> Tightener {
        Tightener::new()
    }
}

impl Tightener {
    pub fn new() -> Tightener {
        Tightener {
            size: (0, 0),
            cols: 0,
            hashes: Vec::new(),
            changed: Vec::new(),
            before: Vec::new(),
        }
    }

    /// Forget everything. The next pass reports every damaged cell, which is
    /// what a picture nothing has seen yet needs.
    pub fn reset(&mut self, size: (u32, u32)) {
        self.size = size;
        self.cols = (size.0 as usize).div_ceil(SEGMENT as usize);
        let cells = self.cols * size.1 as usize;
        self.hashes.clear();
        self.hashes.resize(cells, UNKNOWN);
        self.changed.clear();
        self.changed.resize(cells.div_ceil(64), 0);
        self.before.clear();
    }

    /// Narrow `damage` to the cells that differ, and find the scroll in it
    /// when the backend did not report one.
    pub fn tighten(&mut self, fb: &Framebuffer, damage: &Region, moves: &[Move]) -> Tightened {
        if (fb.width(), fb.height()) != self.size {
            self.reset((fb.width(), fb.height()));
        }
        if self.hashes.is_empty() {
            return Tightened::default();
        }

        // A backend that knows its own moves is believed rather than second
        // guessed, and the cells under the destinations are brought up to
        // date so the block that moved does not also go out as pixels.
        for m in moves {
            self.follow(fb, m);
        }

        let bounds = damage.bounds().intersection(&fb.bounds());
        let span = self.columns(bounds);
        // Detection runs over the whole damage at once, not per rectangle:
        // X11 hands over one scroll as a hundred banded rectangles, and a
        // detector looking at them one at a time would see a hundred pieces
        // of nothing.
        let detecting = moves.is_empty() && bounds.height() > MIN_RUN && !span.is_empty();
        if detecting {
            self.snapshot(bounds, &span);
        }

        self.changed.fill(0);
        for rect in damage.rects() {
            let rect = rect.intersection(&fb.bounds());
            if rect.is_empty() {
                continue;
            }
            // The whole cell is hashed even where the rectangle covers only
            // part of it: the cell is the unit, and a rectangle that ends
            // mid-cell would otherwise hash something the next one hashes
            // differently. Two rectangles can share a cell, which costs one
            // extra hash of the same bytes and changes no answer.
            for y in rect.y1..rect.y2 {
                for c in self.columns(rect) {
                    if self.rehash(fb, y, c) {
                        let cell = y as usize * self.cols + c;
                        self.changed[cell / 64] |= 1 << (cell % 64);
                    }
                }
            }
        }

        let found = if detecting {
            self.detect(bounds, &span)
        } else {
            Vec::new()
        };
        let moves = if found.is_empty() { moves.to_vec() } else { found };

        let mut damage = self.region(bounds, &span);
        for m in &moves {
            // The copy reproduces these pixels, so sending them as well
            // would be paying twice. A session that cannot use the copy puts
            // the destination back as damage itself.
            damage.remove(m.dst);
        }
        Tightened { damage, moves }
    }

    /// The hash cells one rectangle touches.
    fn columns(&self, r: Rect) -> Range<usize> {
        if r.is_empty() {
            return 0..0;
        }
        let first = (r.x1 / SEGMENT).max(0) as usize;
        let last = (((r.x2 + SEGMENT - 1) / SEGMENT) as usize).min(self.cols);
        first..last.max(first)
    }

    /// Hash one cell from the picture and store it. Returns whether it moved.
    fn rehash(&mut self, fb: &Framebuffer, y: i32, c: usize) -> bool {
        let x = c as i32 * SEGMENT;
        let width = SEGMENT.min(self.size.0 as i32 - x);
        if width <= 0 {
            return false;
        }
        let hash = xxh3_64(fb.row_span(y as u32, x as u32, width as u32));
        let cell = y as usize * self.cols + c;
        let moved = self.hashes[cell] != hash;
        self.hashes[cell] = hash;
        moved
    }

    /// Bring a reported move's destination up to date with the picture.
    ///
    /// The alternative is copying the source's cells over, which saves the
    /// hashing, but it is only right when the block really did arrive
    /// unchanged and only possible when both edges sit on a cell boundary.
    /// Hashing the destination is true whatever the backend did, and no
    /// backend reports moves at volume today; the shortcut is on the backlog
    /// against the day one does.
    fn follow(&mut self, fb: &Framebuffer, m: &Move) {
        let dst = m.dst.intersection(&fb.bounds());
        if dst.is_empty() {
            return;
        }
        for y in dst.y1..dst.y2 {
            for c in self.columns(dst) {
                self.rehash(fb, y, c);
            }
        }
    }

    /// Keep the bounding box's cells as they are now, before the rehash
    /// overwrites them, since that is the "before" the detector needs.
    fn snapshot(&mut self, bounds: Rect, span: &Range<usize>) {
        self.before.clear();
        self.before.reserve(span.len() * bounds.height() as usize);
        for y in bounds.y1..bounds.y2 {
            let base = y as usize * self.cols;
            self.before
                .extend_from_slice(&self.hashes[base + span.start..base + span.end]);
        }
    }

    fn was(&self, bounds: Rect, span: &Range<usize>, row: i32) -> &[u64] {
        let width = span.len();
        &self.before[(row - bounds.y1) as usize * width..][..width]
    }

    fn now(&self, span: &Range<usize>, row: i32) -> &[u64] {
        let base = row as usize * self.cols;
        &self.hashes[base + span.start..base + span.end]
    }

    fn row_changed(&self, span: &Range<usize>, row: i32) -> bool {
        span.clone().any(|c| {
            let cell = row as usize * self.cols + c;
            self.changed[cell / 64] >> (cell % 64) & 1 != 0
        })
    }

    /// Find the vertical shift that explains the damage, if one does.
    ///
    /// Cell by cell, not row by row. A real scroll shares its rows with
    /// something that did not scroll, a scrollbar at least and usually a
    /// sidebar and a window frame, so a detector that asks "is this whole
    /// row the old row from further down" answers no to every scroll there
    /// is. Asking it of each cell finds the scrolling part and, as a bonus,
    /// says exactly how wide it was.
    ///
    /// Nothing here has to be right for the picture to be right. Every
    /// rectangle that comes out is checked cell against cell before it is
    /// emitted, so the worst a bad guess can do is copy pixels that were
    /// going to be sent anyway.
    fn detect(&self, bounds: Rect, span: &Range<usize>) -> Vec<Move> {
        let changed: Vec<i32> = (bounds.y1..bounds.y2)
            .filter(|&y| self.row_changed(span, y))
            .collect();
        if changed.len() < MIN_RUN as usize {
            return Vec::new();
        }

        // A handful of changed rows spread through the box is enough to find
        // the shift: a scroll moves every row of itself by the same amount,
        // so any one of them that came from somewhere names it.
        let step = changed.len().div_ceil(PROBES).max(1);
        let probes: Vec<i32> = changed.iter().copied().step_by(step).take(PROBES).collect();
        let cells: Vec<&[u64]> = probes.iter().map(|&y| self.now(span, y)).collect();

        // Every row of the old picture against every probe: where they
        // agree, the distance between them is a candidate shift. A flat
        // background agrees with itself at every distance, which adds the
        // same count to every candidate and so changes no winner.
        let mut votes: HashMap<i32, usize> = HashMap::new();
        for y in bounds.y1..bounds.y2 {
            let was = self.was(bounds, span, y);
            for (probe, &row) in probes.iter().enumerate() {
                if y == row {
                    continue;
                }
                let agree = was.iter().zip(cells[probe]).filter(|(a, b)| a == b).count();
                if agree > 0 {
                    *votes.entry(y - row).or_default() += agree;
                }
            }
        }
        let Some((&dy, &count)) = votes.iter().max_by_key(|(_, count)| **count) else {
            return Vec::new();
        };
        if count < MIN_VOTES {
            return Vec::new();
        }

        // Now check that shift properly: the cells that really do hold what
        // sat `dy` rows away, as a region, which bands them into the
        // rectangles a CopyRect can carry.
        let width = self.size.0 as i32;
        let mut rows: Vec<(i32, Vec<(i32, i32)>)> = Vec::new();
        for y in bounds.y1..bounds.y2 {
            let source = y + dy;
            if source < bounds.y1 || source >= bounds.y2 {
                continue;
            }
            let (now, was) = (self.now(span, y), self.was(bounds, span, source));
            let mut spans: Vec<(i32, i32)> = Vec::new();
            let mut open: Option<i32> = None;
            for (k, c) in span.clone().enumerate() {
                match (now[k] == was[k], open) {
                    (true, None) => open = Some(c as i32 * SEGMENT),
                    (false, Some(x1)) => {
                        spans.push((x1, (c as i32 * SEGMENT).min(width)));
                        open = None;
                    }
                    _ => {}
                }
            }
            if let Some(x1) = open {
                spans.push((x1, (span.end as i32 * SEGMENT).min(width)));
            }
            if !spans.is_empty() {
                rows.push((y, spans));
            }
        }

        let mut moves: Vec<Move> = Region::from_rows(rows)
            .rects()
            .iter()
            .filter(|r| r.height() >= MIN_RUN && self.holds_change(span, r))
            .map(|r| Move {
                src_x: r.x1,
                src_y: r.y1 + dy,
                dst: *r,
            })
            .collect();
        // A client applies these in order onto one picture, so a copy must
        // not land on another's source before that one has been read. With
        // the content moving down, the lowest block goes first.
        if dy < 0 {
            moves.reverse();
        }
        moves
    }

    /// Whether anything inside this rectangle actually changed. A copy of
    /// pixels nobody was going to be sent is correct and pointless, and
    /// every one of them costs a rectangle in the update.
    fn holds_change(&self, span: &Range<usize>, r: &Rect) -> bool {
        let first = (r.x1 / SEGMENT).max(0) as usize;
        let last = ((r.x2 + SEGMENT - 1) / SEGMENT) as usize;
        (r.y1..r.y2).any(|y| {
            (first.max(span.start)..last.min(span.end)).any(|c| {
                let cell = y as usize * self.cols + c;
                self.changed[cell / 64] >> (cell % 64) & 1 != 0
            })
        })
    }

    /// The changed cells, as the region a session will encode.
    fn region(&self, bounds: Rect, span: &Range<usize>) -> Region {
        let width = self.size.0 as i32;
        let mut rows: Vec<(i32, Vec<(i32, i32)>)> = Vec::new();
        for y in bounds.y1..bounds.y2 {
            let mut spans: Vec<(i32, i32)> = Vec::new();
            let mut open: Option<i32> = None;
            for c in span.clone() {
                let cell = y as usize * self.cols + c;
                let on = self.changed[cell / 64] >> (cell % 64) & 1 != 0;
                match (on, open) {
                    (true, None) => open = Some(c as i32 * SEGMENT),
                    (false, Some(x1)) => {
                        spans.push((x1, (c as i32 * SEGMENT).min(width)));
                        open = None;
                    }
                    _ => {}
                }
            }
            if let Some(x1) = open {
                spans.push((x1, (span.end as i32 * SEGMENT).min(width)));
            }
            if !spans.is_empty() {
                rows.push((y, spans));
            }
        }
        Region::from_rows(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 256;
    const H: u32 = 200;

    /// Content no two rows share, so the detector has something to tell them
    /// apart by. A flat fill would match every shift at once, which is the
    /// case `featureless` exists to throw away.
    fn noise(fb: &mut Framebuffer, rect: Rect, seed: u64) {
        let mut state = seed | 1;
        for y in rect.y1..rect.y2 {
            for x in rect.x1..rect.x2 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = (state >> 33) as u8;
                fb.put_pixel(x as u32, y as u32, [v, v.wrapping_mul(3), v.wrapping_mul(7), 0]);
            }
        }
    }

    fn whole(fb: &Framebuffer) -> Region {
        Region::from_rect(fb.bounds())
    }

    /// What a client holding `old` ends up with: the copies, then the pixels.
    fn replay(old: &Framebuffer, new: &Framebuffer, out: &Tightened) -> Framebuffer {
        let mut client = old.clone();
        for m in &out.moves {
            client.copy_within(m.src_x as u32, m.src_y as u32, m.dst);
        }
        for r in out.damage.rects() {
            for y in r.y1..r.y2 {
                let row = new.row_span(y as u32, r.x1 as u32, r.width() as u32).to_vec();
                client
                    .row_span_mut(y as u32, r.x1 as u32, r.width() as u32)
                    .copy_from_slice(&row);
            }
        }
        client
    }

    #[test]
    fn a_still_picture_is_reported_once() {
        let mut fb = Framebuffer::new(W, H);
        let all = fb.bounds();
        noise(&mut fb, all, 1);
        let mut t = Tightener::new();
        let first = t.tighten(&fb, &whole(&fb), &[]);
        assert_eq!(first.damage.area(), (W * H) as i64, "nothing is known yet");
        // The backend keeps claiming the whole screen; the pass keeps saying
        // no, which is the point of it.
        let second = t.tighten(&fb, &whole(&fb), &[]);
        assert!(second.damage.is_empty(), "{:?}", second.damage);
    }

    #[test]
    fn one_pixel_becomes_one_cell() {
        let mut fb = Framebuffer::new(W, H);
        let all = fb.bounds();
        noise(&mut fb, all, 2);
        let mut t = Tightener::new();
        t.tighten(&fb, &whole(&fb), &[]);

        fb.put_pixel(100, 50, [9, 9, 9, 0]);
        let out = t.tighten(&fb, &whole(&fb), &[]);
        assert_eq!(out.damage.rects(), [Rect::from_corners(64, 50, 128, 51)]);
        assert!(out.moves.is_empty());
    }

    #[test]
    fn a_scroll_inside_a_coarse_rectangle_is_found() {
        let band = Rect::from_corners(0, 40, W as i32, 120);
        let shift = 4;
        let mut fb = Framebuffer::new(W, H);
        let all = fb.bounds();
        noise(&mut fb, all, 3);
        let mut t = Tightener::new();
        t.tighten(&fb, &whole(&fb), &[]);
        let old = fb.clone();

        // The band scrolls up by four rows and four fresh ones arrive at the
        // bottom, and the backend says only "the band changed".
        fb.copy_within(
            0,
            (band.y1 + shift) as u32,
            Rect::from_corners(0, band.y1, W as i32, band.y2 - shift),
        );
        noise(
            &mut fb,
            Rect::from_corners(0, band.y2 - shift, W as i32, band.y2),
            4,
        );
        let out = t.tighten(&fb, &Region::from_rect(band), &[]);

        assert_eq!(out.moves.len(), 1, "{:?}", out.moves);
        let m = out.moves[0];
        assert_eq!(m.src_y, band.y1 + shift);
        assert_eq!(m.dst, Rect::from_corners(0, band.y1, W as i32, band.y2 - shift));
        assert_eq!(
            out.damage.bounds(),
            Rect::from_corners(0, band.y2 - shift, W as i32, band.y2),
            "only the fresh rows are left: {:?}",
            out.damage
        );
        // The contract the client depends on: copies then pixels reproduce
        // the picture exactly.
        assert_eq!(replay(&old, &fb, &out), fb);
    }

    #[test]
    fn a_reported_move_leaves_nothing_behind() {
        let block = Rect::from_corners(64, 20, 192, 100);
        let mut fb = Framebuffer::new(W, H);
        let all = fb.bounds();
        noise(&mut fb, all, 5);
        let mut t = Tightener::new();
        t.tighten(&fb, &whole(&fb), &[]);

        fb.copy_within(64, 100, block);
        let m = Move {
            src_x: 64,
            src_y: 100,
            dst: block,
        };
        let out = t.tighten(&fb, &Region::from_rect(block), std::slice::from_ref(&m));
        assert_eq!(out.moves, [m], "a backend's own move is passed through");
        assert!(out.damage.is_empty(), "the copy carries it: {:?}", out.damage);

        // The hashes followed the block, so claiming it again changes nothing.
        let again = t.tighten(&fb, &Region::from_rect(block), &[]);
        assert!(again.damage.is_empty(), "{:?}", again.damage);
        assert!(again.moves.is_empty());
    }

    #[test]
    fn a_resize_forgets_everything() {
        let mut fb = Framebuffer::new(W, H);
        let all = fb.bounds();
        noise(&mut fb, all, 6);
        let mut t = Tightener::new();
        t.tighten(&fb, &whole(&fb), &[]);
        assert!(t.tighten(&fb, &whole(&fb), &[]).damage.is_empty());

        fb.resize(128, 100);
        let all = fb.bounds();
        noise(&mut fb, all, 7);
        let out = t.tighten(&fb, &whole(&fb), &[]);
        assert_eq!(out.damage.area(), 128 * 100, "a new picture is all new");
    }

    #[test]
    fn a_shift_too_small_to_be_sure_of_is_not_claimed() {
        // Four rows of a flat colour moved: featureless, and shorter than a
        // copy is worth. Nothing should be claimed.
        let mut fb = Framebuffer::new(W, H);
        let all = fb.bounds();
        fb.fill(all, [7, 7, 7, 0]);
        let mut t = Tightener::new();
        t.tighten(&fb, &whole(&fb), &[]);

        let band = Rect::from_corners(0, 10, W as i32, 30);
        fb.fill(Rect::from_corners(0, 12, W as i32, 16), [200, 30, 30, 0]);
        let out = t.tighten(&fb, &Region::from_rect(band), &[]);
        assert!(out.moves.is_empty(), "{:?}", out.moves);
        assert_eq!(out.damage.bounds(), Rect::from_corners(0, 12, W as i32, 16));
    }
}
