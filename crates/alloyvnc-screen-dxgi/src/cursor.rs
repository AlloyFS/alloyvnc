//! DXGI pointer shapes into RGBA.
//!
//! Three kinds arrive. Colour is BGRA with a real alpha channel. Masked
//! colour is BGRA where the fourth byte is a mask: zero means "draw the
//! colour", 0xFF means "XOR the colour onto the screen". Monochrome is two
//! one-bit planes stacked vertically, an AND mask over an XOR mask, the
//! Windows 3.x cursor format that the I-beam still uses. XOR pixels invert
//! whatever is under them; a VNC client cannot do that, so they are drawn
//! opaque instead.
//!
//! Pure, so it is tested on every platform.

use alloyvnc_screen::CursorShape;

pub const MONOCHROME: u32 = 1;
pub const COLOR: u32 = 2;
pub const MASKED_COLOR: u32 = 4;

/// `height` and `pitch` are as DXGI reports them: for a monochrome shape
/// the height covers both planes.
pub fn convert(kind: u32, width: u32, height: u32, pitch: u32, hot: (u32, u32), data: &[u8]) -> CursorShape {
    let at = |i: usize| data.get(i).copied().unwrap_or(0);
    let (w, p) = (width as usize, pitch as usize);
    match kind {
        COLOR | MASKED_COLOR => {
            let h = height as usize;
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let i = y * p + x * 4;
                    let (b, g, r, a) = (at(i), at(i + 1), at(i + 2), at(i + 3));
                    let alpha = if kind == COLOR {
                        a
                    } else if a == 0 {
                        255
                    } else if (r, g, b) == (0, 0, 0) {
                        // XOR with nothing: the screen shows through.
                        0
                    } else {
                        255
                    };
                    rgba.extend_from_slice(&[r, g, b, alpha]);
                }
            }
            CursorShape::new(width, h as u32, hot.0, hot.1, rgba)
        }
        MONOCHROME => {
            let h = (height / 2) as usize;
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let bit = 0x80 >> (x % 8);
                    let and = at(y * p + x / 8) & bit != 0;
                    let xor = at((y + h) * p + x / 8) & bit != 0;
                    let px = match (and, xor) {
                        (false, false) => [0, 0, 0, 255],
                        (false, true) => [255, 255, 255, 255],
                        (true, false) => [0, 0, 0, 0],
                        (true, true) => [255, 255, 255, 255],
                    };
                    rgba.extend_from_slice(&px);
                }
            }
            CursorShape::new(width, h as u32, hot.0, hot.1, rgba)
        }
        _ => CursorShape::hidden(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_swaps_to_rgba_and_honours_the_pitch() {
        // Two pixels per row, a pitch of 12 bytes (four of padding).
        let data = [
            10, 20, 30, 255, 40, 50, 60, 0, 9, 9, 9, 9, 70, 80, 90, 128, 0, 0, 0, 0, 9, 9, 9, 9,
        ];
        let s = convert(COLOR, 2, 2, 12, (1, 0), &data);
        assert_eq!((s.width, s.height, s.hot_x, s.hot_y), (2, 2, 1, 0));
        assert_eq!(
            s.rgba,
            [30, 20, 10, 255, 60, 50, 40, 0, 90, 80, 70, 128, 0, 0, 0, 0]
        );
    }

    #[test]
    fn masked_colour_mask_becomes_alpha() {
        let data = [1, 2, 3, 0, 0, 0, 0, 255, 5, 6, 7, 255];
        let s = convert(MASKED_COLOR, 3, 1, 12, (0, 0), &data);
        assert_eq!(s.rgba, [3, 2, 1, 255, 0, 0, 0, 0, 7, 6, 5, 255]);
    }

    #[test]
    fn monochrome_planes() {
        // 8 wide, 1 high: AND row then XOR row, one byte each.
        // AND 0b0011_0000, XOR 0b0101_0000: pixels are black, white, transparent, inverted, then black.
        let data = [0b0011_0000, 0b0101_0000];
        let s = convert(MONOCHROME, 8, 2, 1, (0, 0), &data);
        assert_eq!(s.height, 1);
        assert_eq!(
            &s.rgba[..16],
            &[0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 255, 255, 255, 255]
        );
        assert_eq!(&s.rgba[16..20], &[0, 0, 0, 255]);
    }

    #[test]
    fn short_data_and_unknown_kinds_do_not_panic() {
        let s = convert(COLOR, 4, 4, 16, (0, 0), &[1, 2]);
        assert_eq!(s.rgba.len(), 64);
        assert!(convert(9, 4, 4, 16, (0, 0), &[]).is_hidden());
    }
}
