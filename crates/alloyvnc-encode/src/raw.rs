//! Raw (encoding 0): the pixels, row by row, in the client's format. The
//! baseline every other encoder is measured against, and the right answer on
//! a loopback or a fast LAN, where the CPU is worth more than the bytes.

use alloyvnc_proto::PixelFormat;
use alloyvnc_region::Rect;

use crate::Framebuffer;
use crate::convert::convert_row;

/// Append `rect` of `fb` to `out` in the client's format. The rectangle is
/// clipped to the picture; the caller wrote the header for what it asked.
pub fn encode(fb: &Framebuffer, rect: Rect, pf: &PixelFormat, out: &mut Vec<u8>) {
    let rect = rect.intersection(&fb.bounds());
    if rect.is_empty() {
        return;
    }
    out.reserve(rect.area() as usize * pf.bytes_per_pixel());
    for y in rect.y1..rect.y2 {
        let src = fb.row_span(y as u32, rect.x1 as u32, rect.width() as u32);
        convert_row(src, pf, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_rect_is_the_rows_concatenated() {
        let mut fb = Framebuffer::new(4, 3);
        for y in 0..3 {
            for x in 0..4 {
                fb.put_pixel(x, y, [x as u8, y as u8, 7, 0]);
            }
        }
        let mut out = Vec::new();
        encode(&fb, Rect::new(1, 1, 2, 2), &PixelFormat::bgrx32(), &mut out);
        assert_eq!(out, [1, 1, 7, 0, 2, 1, 7, 0, 1, 2, 7, 0, 2, 2, 7, 0]);

        out.clear();
        encode(&fb, Rect::new(3, 2, 5, 5), &PixelFormat::bgrx32(), &mut out);
        assert_eq!(out, [3, 2, 7, 0], "clipped to the picture");

        out.clear();
        encode(&fb, Rect::new(0, 0, 1, 1), &PixelFormat::bgr233(), &mut out);
        assert_eq!(out, [0b00_000_000], "converted: one byte per pixel");
    }
}
