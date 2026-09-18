//! Where a monitor's rectangles land in the picture.
//!
//! DXGI reports in the monitor's own coordinates, starting at its top-left
//! corner; the picture puts every monitor side by side with its origin at
//! the top-left of the virtual desktop. The translation is an addition, and
//! the only part with a trap in it is the clipping, which is why it is here
//! rather than inline: this is arithmetic, so it is tested on every
//! platform rather than only on the machine with a screen.

use alloyvnc_screen::{Move, Rect};

/// One of DXGI's move rectangles, clipped to what the picture can hold and
/// translated into picture coordinates. `None` when nothing of it is left.
///
/// `reach` is the part of the monitor that lands inside the picture, in the
/// monitor's own coordinates, and `off` is where the monitor's top-left
/// sits in the picture. Clipping the destination has to take the same bite
/// out of the source: a block whose left edge is cut by ten pixels now
/// starts ten pixels further into the source too, and moving it without
/// that shows the wrong ten pixels down its whole height.
pub fn clip_move(src: (i32, i32), dst: Rect, reach: Rect, off: (i32, i32)) -> Option<Move> {
    let clipped = dst.intersection(&reach);
    if clipped.is_empty() {
        return None;
    }
    Some(Move {
        src_x: src.0 + (clipped.x1 - dst.x1) + off.0,
        src_y: src.1 + (clipped.y1 - dst.y1) + off.1,
        dst: clipped.translate(off.0, off.1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REACH: Rect = Rect::from_corners(0, 0, 100, 100);

    #[test]
    fn a_move_inside_the_monitor_is_only_translated() {
        let m = clip_move((10, 20), Rect::new(30, 40, 20, 10), REACH, (1920, 0)).unwrap();
        assert_eq!(m.src_x, 1930);
        assert_eq!(m.src_y, 20);
        assert_eq!(m.dst, Rect::new(1950, 40, 20, 10));
        // The source keeps the destination's size, which is what CopyRect
        // carries.
        assert_eq!(m.src().width(), m.dst.width());
    }

    #[test]
    fn clipping_the_destination_moves_the_source_by_as_much() {
        // Ten pixels off the left and five off the top.
        let m = clip_move((50, 50), Rect::from_corners(-10, -5, 40, 30), REACH, (0, 0)).unwrap();
        assert_eq!(m.dst, Rect::from_corners(0, 0, 40, 30));
        assert_eq!((m.src_x, m.src_y), (60, 55));
    }

    #[test]
    fn the_far_edges_clip_without_touching_the_source() {
        // Nothing is cut off the near side, so the source is where it was.
        let m = clip_move((10, 10), Rect::from_corners(80, 80, 140, 130), REACH, (0, 0)).unwrap();
        assert_eq!(m.dst, Rect::from_corners(80, 80, 100, 100));
        assert_eq!((m.src_x, m.src_y), (10, 10));
    }

    #[test]
    fn a_move_with_nothing_left_is_dropped() {
        assert!(clip_move((0, 0), Rect::new(200, 200, 10, 10), REACH, (0, 0)).is_none());
        assert!(clip_move((0, 0), Rect::EMPTY, REACH, (0, 0)).is_none());
        assert!(clip_move((0, 0), Rect::new(0, 0, 10, 10), Rect::EMPTY, (0, 0)).is_none());
    }
}
