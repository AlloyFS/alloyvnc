//! XFixes cursor images into straight RGBA.
//!
//! XFixes hands the pointer over as one 32-bit word per pixel in the
//! server's own byte order, laid out as alpha, red, green, blue from the top
//! down, with the colour **premultiplied**: a half-transparent red pixel is
//! stored as (128, 128, 0, 0) rather than (128, 255, 0, 0), because that is
//! the form the X Render extension composites in and the cursor is a Render
//! picture. RFB's CursorWithAlpha wants straight alpha, so every channel is
//! divided back out.
//!
//! Pure, so it is tested on every platform.

use alloyvnc_screen::CursorShape;

/// One XFixes cursor image. `argb` is `width * height` words; anything
/// shorter is treated as transparent, so a short reply cannot panic.
pub fn convert(width: u16, height: u16, hot: (u16, u16), argb: &[u32]) -> CursorShape {
    if width == 0 || height == 0 {
        return CursorShape::hidden();
    }
    let count = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(count * 4);
    for i in 0..count {
        let px = argb.get(i).copied().unwrap_or(0);
        let a = (px >> 24) as u8;
        let (r, g, b) = ((px >> 16) as u8, (px >> 8) as u8, px as u8);
        rgba.extend_from_slice(&[straighten(r, a), straighten(g, a), straighten(b, a), a]);
    }
    CursorShape::new(width as u32, height as u32, hot.0 as u32, hot.1 as u32, rgba)
}

/// One channel with the alpha divided back out. Rounded rather than
/// truncated, so a fully opaque pixel survives the round trip exactly and a
/// faint one does not lose its last level.
fn straighten(value: u8, alpha: u8) -> u8 {
    match alpha {
        0 => 0,
        255 => value,
        a => {
            let widened = value as u32 * 255 + a as u32 / 2;
            (widened / a as u32).min(255) as u8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_pixels_pass_through_unchanged() {
        let argb = [0xff_11_22_33, 0xff_00_00_00];
        let shape = convert(2, 1, (0, 0), &argb);
        assert_eq!((shape.width, shape.height), (2, 1));
        assert_eq!(shape.rgba, [0x11, 0x22, 0x33, 0xff, 0, 0, 0, 0xff]);
    }

    #[test]
    fn a_half_transparent_red_comes_back_full_red() {
        // Premultiplied: alpha 128 over pure red is stored as 128, not 255.
        let shape = convert(1, 1, (0, 0), &[0x80_80_00_00]);
        assert_eq!(shape.rgba, [255, 0, 0, 0x80]);
    }

    #[test]
    fn a_fully_transparent_pixel_has_no_colour_to_recover() {
        let shape = convert(1, 1, (0, 0), &[0x00_00_00_00]);
        assert_eq!(shape.rgba, [0, 0, 0, 0]);
    }

    #[test]
    fn the_hot_spot_and_the_size_are_carried() {
        let shape = convert(4, 3, (2, 1), &[0xffff_ffff; 12]);
        assert_eq!(
            (shape.width, shape.height, shape.hot_x, shape.hot_y),
            (4, 3, 2, 1)
        );
        assert_eq!(shape.rgba.len(), 4 * 3 * 4);
        assert!(!shape.is_hidden());
    }

    #[test]
    fn a_short_reply_and_an_empty_cursor_do_not_panic() {
        let shape = convert(4, 4, (0, 0), &[0xffff_ffff]);
        assert_eq!(shape.rgba.len(), 64);
        assert_eq!(&shape.rgba[4..8], &[0, 0, 0, 0]);
        assert!(convert(0, 0, (0, 0), &[]).is_hidden());
    }
}
