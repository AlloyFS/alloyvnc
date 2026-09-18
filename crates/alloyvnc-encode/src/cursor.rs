//! The pointer, sent as a shape rather than drawn into the picture.
//!
//! Two pseudo-encodings carry it. Cursor (-239) is the old one: pixels in
//! the client's own format plus a one-bit mask, rows padded to bytes.
//! CursorWithAlpha (-314) is TigerVNC's: an encoding number, then RGBA
//! pixels with a straight (not premultiplied) alpha channel. A shape with no
//! size hides the pointer.

use alloyvnc_proto::PixelFormat;

use crate::convert::convert_row;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// `width * height` pixels, four bytes each: R, G, B, A.
    pub rgba: Vec<u8>,
}

impl CursorShape {
    pub fn new(width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8>) -> CursorShape {
        assert_eq!(
            rgba.len(),
            width as usize * height as usize * 4,
            "cursor pixels do not match its size"
        );
        CursorShape {
            width,
            height,
            hot_x,
            hot_y,
            rgba,
        }
    }

    /// No pointer at all.
    pub fn hidden() -> CursorShape {
        CursorShape {
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            rgba: Vec::new(),
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Cursor (-239): the pixels in `pf`, then the mask.
pub fn encode_cursor(shape: &CursorShape, pf: &PixelFormat, out: &mut Vec<u8>) {
    let w = shape.width as usize;
    let mut row = Vec::with_capacity(w * 4);
    for line in shape.rgba.chunks_exact(w * 4) {
        row.clear();
        for px in line.chunks_exact(4) {
            row.extend_from_slice(&[px[2], px[1], px[0], 0]);
        }
        convert_row(&row, pf, out);
    }
    let mask_stride = w.div_ceil(8);
    for line in shape.rgba.chunks_exact(w * 4) {
        let start = out.len();
        out.resize(start + mask_stride, 0);
        for (x, px) in line.chunks_exact(4).enumerate() {
            if px[3] >= 128 {
                out[start + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
}

/// CursorWithAlpha (-314): Raw, then RGBA.
pub fn encode_cursor_with_alpha(shape: &CursorShape, out: &mut Vec<u8>) {
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&shape.rgba);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrow() -> CursorShape {
        // 9 pixels wide so the mask needs a second byte per row.
        let mut rgba = vec![0u8; 9 * 2 * 4];
        for x in 0..9 {
            rgba[x * 4..x * 4 + 4].copy_from_slice(&[255, 0, 0, if x % 2 == 0 { 255 } else { 0 }]);
        }
        rgba[9 * 4..9 * 4 + 4].copy_from_slice(&[0, 0, 255, 200]);
        CursorShape::new(9, 2, 1, 1, rgba)
    }

    #[test]
    fn cursor_pixels_then_mask() {
        let mut out = Vec::new();
        encode_cursor(&arrow(), &PixelFormat::bgrx32(), &mut out);
        assert_eq!(out.len(), 9 * 2 * 4 + 2 * 2);
        assert_eq!(&out[..4], &[0, 0, 255, 0], "first pixel is red in BGRX");
        assert_eq!(&out[9 * 4..9 * 4 + 4], &[255, 0, 0, 0], "second row starts blue");
        assert_eq!(&out[72..], &[0b1010_1010, 0b1000_0000, 0b1000_0000, 0]);
    }

    #[test]
    fn cursor_with_alpha_is_raw_rgba() {
        let shape = arrow();
        let mut out = Vec::new();
        encode_cursor_with_alpha(&shape, &mut out);
        assert_eq!(&out[..4], &[0, 0, 0, 0]);
        assert_eq!(&out[4..], &shape.rgba[..]);
        assert!(CursorShape::hidden().is_hidden());
        assert!(!shape.is_hidden());
    }
}
