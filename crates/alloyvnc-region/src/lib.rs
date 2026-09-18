//! Rectangles and regions for damage tracking.
//!
//! A [`Region`] is a set of pixels kept as non-overlapping rectangles in y-x
//! banded form: sorted by top edge then left edge, every rectangle in a band
//! sharing the same top and bottom, and neighbouring bands merged when their
//! horizontal spans are identical. That normal form makes union,
//! intersection and subtraction one walk over the bands, and the rectangle
//! list a region hands to an encoder is the smallest one for its shape under
//! that banding.
//!
//! Coordinates are half-open: a rectangle covers `x1..x2` by `y1..y2`.

#![forbid(unsafe_code)]

/// A half-open rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect {
        x1: 0,
        y1: 0,
        x2: 0,
        y2: 0,
    };

    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x1: x,
            y1: y,
            x2: x + width,
            y2: y + height,
        }
    }

    pub const fn from_corners(x1: i32, y1: i32, x2: i32, y2: i32) -> Rect {
        Rect { x1, y1, x2, y2 }
    }

    pub const fn width(&self) -> i32 {
        if self.x2 > self.x1 { self.x2 - self.x1 } else { 0 }
    }

    pub const fn height(&self) -> i32 {
        if self.y2 > self.y1 { self.y2 - self.y1 } else { 0 }
    }

    pub const fn is_empty(&self) -> bool {
        self.x2 <= self.x1 || self.y2 <= self.y1
    }

    pub const fn area(&self) -> i64 {
        self.width() as i64 * self.height() as i64
    }

    /// The overlap, or [`Rect::EMPTY`].
    pub fn intersection(&self, other: &Rect) -> Rect {
        Rect {
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
            x2: self.x2.min(other.x2),
            y2: self.y2.min(other.y2),
        }
        .normalised()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        !self.intersection(other).is_empty()
    }

    /// Whether every pixel of `other` is inside this rectangle. An empty
    /// `other` is inside everything.
    pub fn contains(&self, other: &Rect) -> bool {
        other.is_empty()
            || (self.x1 <= other.x1 && self.y1 <= other.y1 && other.x2 <= self.x2 && other.y2 <= self.y2)
    }

    pub const fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.x1 && x < self.x2 && y >= self.y1 && y < self.y2
    }

    /// The smallest rectangle holding both. An empty side is ignored.
    pub fn union_bounds(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return other.normalised();
        }
        if other.is_empty() {
            return self.normalised();
        }
        Rect {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    pub const fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            x1: self.x1 + dx,
            y1: self.y1 + dy,
            x2: self.x2 + dx,
            y2: self.y2 + dy,
        }
    }

    /// Every empty rectangle as the one [`Rect::EMPTY`], so they compare equal.
    pub fn normalised(self) -> Rect {
        if self.is_empty() { Rect::EMPTY } else { self }
    }
}

/// A block that moved: `dst` now holds what was at `(src_x, src_y)`, a block
/// of `dst`'s size. What CopyRect carries, and what a compositor reports for
/// a scroll or a window drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Move {
    pub src_x: i32,
    pub src_y: i32,
    pub dst: Rect,
}

impl Move {
    pub fn src(&self) -> Rect {
        Rect::new(self.src_x, self.src_y, self.dst.width(), self.dst.height())
    }

    pub fn translate(&self, dx: i32, dy: i32) -> Move {
        Move {
            src_x: self.src_x + dx,
            src_y: self.src_y + dy,
            dst: self.dst.translate(dx, dy),
        }
    }
}

/// A horizontal span `x1..x2` inside one band.
type Span = (i32, i32);

/// A set of pixels as y-x banded rectangles. See the crate docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    rects: Vec<Rect>,
}

#[derive(Clone, Copy)]
enum Op {
    Union,
    Intersect,
    Subtract,
}

impl Region {
    pub const fn new() -> Region {
        Region { rects: Vec::new() }
    }

    pub fn from_rect(rect: Rect) -> Region {
        if rect.is_empty() {
            Region::new()
        } else {
            Region { rects: vec![rect] }
        }
    }

    pub fn from_rects<I: IntoIterator<Item = Rect>>(rects: I) -> Region {
        let mut out = Region::new();
        for r in rects {
            out.add(r);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The rectangles, in band order.
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    pub fn len(&self) -> usize {
        self.rects.len()
    }

    pub fn bounds(&self) -> Rect {
        self.rects.iter().fold(Rect::EMPTY, |b, r| b.union_bounds(r))
    }

    pub fn area(&self) -> i64 {
        self.rects.iter().map(Rect::area).sum()
    }

    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        self.rects.iter().any(|r| r.contains_point(x, y))
    }

    pub fn intersects_rect(&self, rect: &Rect) -> bool {
        self.rects.iter().any(|r| r.intersects(rect))
    }

    pub fn union(&self, other: &Region) -> Region {
        op(self, other, Op::Union)
    }

    pub fn intersect(&self, other: &Region) -> Region {
        op(self, other, Op::Intersect)
    }

    pub fn subtract(&self, other: &Region) -> Region {
        op(self, other, Op::Subtract)
    }

    pub fn intersect_rect(&self, rect: &Rect) -> Region {
        self.intersect(&Region::from_rect(*rect))
    }

    /// Add a rectangle in place.
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        *self = self.union(&Region::from_rect(rect));
    }

    /// Remove a rectangle in place.
    pub fn remove(&mut self, rect: Rect) {
        if rect.is_empty() || self.is_empty() {
            return;
        }
        *self = self.subtract(&Region::from_rect(rect));
    }

    pub fn translate(&self, dx: i32, dy: i32) -> Region {
        Region {
            rects: self.rects.iter().map(|r| r.translate(dx, dy)).collect(),
        }
    }

    pub fn clear(&mut self) {
        self.rects.clear();
    }
}

fn op(a: &Region, b: &Region, kind: Op) -> Region {
    match kind {
        Op::Union => {
            if a.is_empty() {
                return b.clone();
            }
            if b.is_empty() {
                return a.clone();
            }
        }
        Op::Intersect => {
            if a.is_empty() || b.is_empty() {
                return Region::new();
            }
        }
        Op::Subtract => {
            if a.is_empty() {
                return Region::new();
            }
            if b.is_empty() {
                return a.clone();
            }
        }
    }

    // Every top and bottom edge of either side splits the plane into bands.
    // Within a band no rectangle starts or ends, so each side is a plain
    // list of horizontal spans and the set operation is one-dimensional.
    let mut edges: Vec<i32> = Vec::with_capacity((a.rects.len() + b.rects.len()) * 2);
    edges.extend(a.rects.iter().flat_map(|r| [r.y1, r.y2]));
    edges.extend(b.rects.iter().flat_map(|r| [r.y1, r.y2]));
    edges.sort_unstable();
    edges.dedup();

    let mut out = Vec::new();
    let mut open: Option<(i32, i32, Vec<Span>)> = None;
    let (mut sa, mut sb, mut spans) = (Vec::new(), Vec::new(), Vec::new());
    for pair in edges.windows(2) {
        let (y1, y2) = (pair[0], pair[1]);
        band_spans(&a.rects, y1, y2, &mut sa);
        band_spans(&b.rects, y1, y2, &mut sb);
        spans.clear();
        match kind {
            Op::Union => union_spans(&sa, &sb, &mut spans),
            Op::Intersect => intersect_spans(&sa, &sb, &mut spans),
            Op::Subtract => subtract_spans(&sa, &sb, &mut spans),
        }
        if spans.is_empty() {
            flush(&mut open, &mut out);
            continue;
        }
        if let Some((_, oy2, ospans)) = &mut open
            && *oy2 == y1
            && *ospans == spans
        {
            *oy2 = y2;
            continue;
        }
        flush(&mut open, &mut out);
        open = Some((y1, y2, spans.clone()));
    }
    flush(&mut open, &mut out);
    Region { rects: out }
}

fn flush(open: &mut Option<(i32, i32, Vec<Span>)>, out: &mut Vec<Rect>) {
    if let Some((y1, y2, spans)) = open.take() {
        out.extend(spans.into_iter().map(|(x1, x2)| Rect { x1, y1, x2, y2 }));
    }
}

/// The spans of `rects` covering the band `y1..y2`. Because the band's edges
/// come from the rectangles themselves, a rectangle either covers the whole
/// band or none of it.
fn band_spans(rects: &[Rect], y1: i32, y2: i32, out: &mut Vec<Span>) {
    out.clear();
    out.extend(
        rects
            .iter()
            .filter(|r| r.y1 <= y1 && r.y2 >= y2)
            .map(|r| (r.x1, r.x2)),
    );
    out.sort_unstable();
}

fn union_spans(a: &[Span], b: &[Span], out: &mut Vec<Span>) {
    let (mut i, mut j) = (0, 0);
    let mut cur: Option<Span> = None;
    loop {
        let next = match (a.get(i), b.get(j)) {
            (Some(&x), Some(&y)) => {
                if x.0 <= y.0 {
                    i += 1;
                    x
                } else {
                    j += 1;
                    y
                }
            }
            (Some(&x), None) => {
                i += 1;
                x
            }
            (None, Some(&y)) => {
                j += 1;
                y
            }
            (None, None) => break,
        };
        match cur {
            Some((cx1, cx2)) if next.0 <= cx2 => cur = Some((cx1, cx2.max(next.1))),
            Some(c) => {
                out.push(c);
                cur = Some(next);
            }
            None => cur = Some(next),
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
}

fn intersect_spans(a: &[Span], b: &[Span], out: &mut Vec<Span>) {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let x1 = a[i].0.max(b[j].0);
        let x2 = a[i].1.min(b[j].1);
        if x1 < x2 {
            out.push((x1, x2));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
}

fn subtract_spans(a: &[Span], b: &[Span], out: &mut Vec<Span>) {
    let mut j = 0;
    for &(ax1, ax2) in a {
        let mut cur = ax1;
        while j < b.len() && b[j].1 <= cur {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && b[k].0 < ax2 {
            if b[k].0 > cur {
                out.push((cur, b[k].0));
            }
            cur = cur.max(b[k].1);
            if cur >= ax2 {
                break;
            }
            k += 1;
        }
        if cur < ax2 {
            out.push((cur, ax2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: usize = 96;

    fn bitmap(region: &Region) -> Vec<bool> {
        let mut bm = vec![false; SIZE * SIZE];
        for r in region.rects() {
            for y in r.y1..r.y2 {
                for x in r.x1..r.x2 {
                    bm[y as usize * SIZE + x as usize] = true;
                }
            }
        }
        bm
    }

    /// The banded normal form, checked directly.
    fn check_invariants(region: &Region) {
        let rects = region.rects();
        for r in rects {
            assert!(!r.is_empty(), "empty rect in {rects:?}");
        }
        for w in rects.windows(2) {
            let (p, q) = (w[0], w[1]);
            assert!((p.y1, p.x1) < (q.y1, q.x1), "not sorted: {rects:?}");
            if p.y1 == q.y1 {
                assert_eq!(p.y2, q.y2, "band with mixed bottoms: {rects:?}");
                assert!(q.x1 > p.x2, "touching or overlapping spans in a band: {rects:?}");
            }
        }
        for (i, p) in rects.iter().enumerate() {
            for q in &rects[i + 1..] {
                assert!(!p.intersects(q), "overlap {p:?} {q:?}");
            }
        }
        // Consecutive bands with identical spans must have been merged.
        let mut bands: Vec<(i32, i32, Vec<Span>)> = Vec::new();
        for r in rects {
            match bands.last_mut() {
                Some((y1, _, spans)) if *y1 == r.y1 => spans.push((r.x1, r.x2)),
                _ => bands.push((r.y1, r.y2, vec![(r.x1, r.x2)])),
            }
        }
        for w in bands.windows(2) {
            if w[0].1 == w[1].0 {
                assert_ne!(w[0].2, w[1].2, "unmerged bands in {rects:?}");
            }
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }

        fn below(&mut self, n: u32) -> i32 {
            (self.next() % n) as i32
        }

        fn rect(&mut self) -> Rect {
            Rect::new(self.below(64), self.below(64), self.below(24), self.below(24))
        }

        fn region(&mut self) -> Region {
            let n = 1 + self.below(5);
            Region::from_rects((0..n).map(|_| self.rect()))
        }
    }

    #[test]
    fn ops_match_a_bitmap_oracle() {
        let mut rng = Lcg(42);
        for _ in 0..1500 {
            let a = rng.region();
            let b = rng.region();
            check_invariants(&a);
            check_invariants(&b);
            let (ba, bb) = (bitmap(&a), bitmap(&b));

            let u = a.union(&b);
            check_invariants(&u);
            let expect: Vec<bool> = ba.iter().zip(&bb).map(|(x, y)| *x || *y).collect();
            assert_eq!(bitmap(&u), expect, "union of {a:?} and {b:?}");

            let i = a.intersect(&b);
            check_invariants(&i);
            let expect: Vec<bool> = ba.iter().zip(&bb).map(|(x, y)| *x && *y).collect();
            assert_eq!(bitmap(&i), expect, "intersection of {a:?} and {b:?}");

            let s = a.subtract(&b);
            check_invariants(&s);
            let expect: Vec<bool> = ba.iter().zip(&bb).map(|(x, y)| *x && !*y).collect();
            assert_eq!(bitmap(&s), expect, "difference of {a:?} and {b:?}");

            assert_eq!(u.area(), expect_area(&bitmap(&u)));
            assert_eq!(
                a.intersects_rect(&b.bounds()),
                a.rects().iter().any(|r| r.intersects(&b.bounds()))
            );
        }
    }

    fn expect_area(bm: &[bool]) -> i64 {
        bm.iter().filter(|&&p| p).count() as i64
    }

    #[test]
    fn rects_merge_into_the_fewest_bands() {
        // Two squares side by side become one rectangle; stacked, one too.
        let side = Region::from_rects([Rect::new(0, 0, 10, 10), Rect::new(10, 0, 10, 10)]);
        assert_eq!(side.rects(), &[Rect::new(0, 0, 20, 10)]);
        let stacked = Region::from_rects([Rect::new(0, 0, 10, 10), Rect::new(0, 10, 10, 10)]);
        assert_eq!(stacked.rects(), &[Rect::new(0, 0, 10, 20)]);
        // An L shape is two bands.
        let l = Region::from_rects([Rect::new(0, 0, 10, 20), Rect::new(0, 10, 20, 10)]);
        assert_eq!(l.rects(), &[Rect::new(0, 0, 10, 10), Rect::new(0, 10, 20, 10)]);
        // A hole.
        let mut holed = Region::from_rect(Rect::new(0, 0, 30, 30));
        holed.remove(Rect::new(10, 10, 10, 10));
        assert_eq!(holed.len(), 4);
        assert_eq!(holed.area(), 800);
        assert!(!holed.contains_point(15, 15));
        assert!(holed.contains_point(5, 15));
    }

    #[test]
    fn rect_basics() {
        let r = Rect::new(10, 20, 30, 40);
        assert_eq!((r.width(), r.height(), r.area()), (30, 40, 1200));
        assert!(Rect::new(0, 0, 0, 5).is_empty());
        assert_eq!(Rect::new(0, 0, 0, 5).normalised(), Rect::EMPTY);
        assert_eq!(
            r.intersection(&Rect::new(30, 30, 100, 100)),
            Rect::from_corners(30, 30, 40, 60)
        );
        assert_eq!(r.intersection(&Rect::new(100, 100, 5, 5)), Rect::EMPTY);
        assert_eq!(r.union_bounds(&Rect::EMPTY), r);
        assert_eq!(Rect::EMPTY.union_bounds(&r), r);
        assert!(r.contains(&Rect::new(10, 20, 1, 1)));
        assert!(!r.contains(&Rect::new(9, 20, 1, 1)));
        assert_eq!(r.translate(-10, -20), Rect::new(0, 0, 30, 40));
        assert!(Region::from_rect(Rect::EMPTY).is_empty());
        assert_eq!(Region::new().bounds(), Rect::EMPTY);
    }
}
