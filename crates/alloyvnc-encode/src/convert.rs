//! Pixel-format conversion: the framebuffer's BGRX into whatever the client
//! asked for.
//!
//! The common case is no conversion at all. A client on a 24-bit display
//! asks for exactly the framebuffer's layout, and [`convert_row`] answers with
//! a copy. The other layouts go through one loop shaped so the compiler can
//! vectorise it; hand-written SIMD waits for a profile that asks for it.
//!
//! There was going to be a third path here, a byte shuffle for the 32-bit
//! layouts that are the framebuffer's own bytes in another order, which is
//! what noVNC asks for. Measured on a 1080p frame, it has nothing to win:
//! shuffled and packed both run at 2.45 GB/s, and a straight copy of the
//! same bytes runs at 5.1, which is the same memory bandwidth once both
//! directions are counted. The packer is already going as fast as the
//! memory will carry it and its arithmetic is free. Put behind a dispatch
//! rather than inlined, the shuffle was half as fast again, which is the
//! other half of the lesson. `examples/convert-bench.rs` keeps all of it
//! side by side.
//!
//! What would repay attention is the narrow formats: rgb565 manages 0.83
//! GB/s, a third of the rate, because every pixel is scaled.

use alloyvnc_proto::PixelFormat;

/// Whether `pf` is the framebuffer's own layout, so pixels go out untouched.
pub fn is_native(pf: &PixelFormat) -> bool {
    *pf == PixelFormat::bgrx32()
}

/// Packs 8-bit BGR into a client's layout. Built once per pixel format, used
/// per pixel.
#[derive(Clone, Copy, Debug)]
pub struct Packer {
    red_max: u32,
    green_max: u32,
    blue_max: u32,
    red_shift: u32,
    green_shift: u32,
    blue_shift: u32,
}

impl Packer {
    pub fn new(pf: &PixelFormat) -> Packer {
        Packer {
            red_max: u32::from(pf.red_max),
            green_max: u32::from(pf.green_max),
            blue_max: u32::from(pf.blue_max),
            red_shift: u32::from(pf.red_shift),
            green_shift: u32::from(pf.green_shift),
            blue_shift: u32::from(pf.blue_shift),
        }
    }

    /// `px` is one BGRX pixel.
    #[inline]
    pub fn pack(&self, px: &[u8]) -> u32 {
        (scale(px[2], self.red_max) << self.red_shift)
            | (scale(px[1], self.green_max) << self.green_shift)
            | (scale(px[0], self.blue_max) << self.blue_shift)
    }
}

/// An 8-bit channel value scaled to `0..=max`, rounded to nearest.
#[inline]
fn scale(c: u8, max: u32) -> u32 {
    if max == 255 {
        u32::from(c)
    } else {
        (u32::from(c) * max + 127) / 255
    }
}

/// Convert one row of packed BGRX pixels into `pf`, appending to `out`.
pub fn convert_row(src: &[u8], pf: &PixelFormat, out: &mut Vec<u8>) {
    if is_native(pf) {
        out.extend_from_slice(src);
        return;
    }
    let packer = Packer::new(pf);
    // Whole pixels only: `as_chunks` hands back arrays of four, so every
    // index into a pixel below is checked once here rather than per pixel.
    let pixels = src.as_chunks::<4>().0.iter();
    out.reserve(pixels.len() * pf.bytes_per_pixel());
    match (pf.bits_per_pixel, pf.big_endian) {
        (8, _) => out.extend(pixels.map(|px| packer.pack(px) as u8)),
        (16, false) => {
            for px in pixels {
                out.extend_from_slice(&(packer.pack(px) as u16).to_le_bytes());
            }
        }
        (16, true) => {
            for px in pixels {
                out.extend_from_slice(&(packer.pack(px) as u16).to_be_bytes());
            }
        }
        (_, false) => {
            for px in pixels {
                out.extend_from_slice(&packer.pack(px).to_le_bytes());
            }
        }
        (_, true) => {
            for px in pixels {
                out.extend_from_slice(&packer.pack(px).to_be_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 4] = [0, 0, 255, 0];
    const GREY: [u8; 4] = [128, 128, 128, 0];

    fn convert(px: [u8; 4], pf: &PixelFormat) -> Vec<u8> {
        let mut out = Vec::new();
        convert_row(&px, pf, &mut out);
        out
    }

    #[test]
    fn native_is_a_copy() {
        let row = [RED, GREY].concat();
        let mut out = Vec::new();
        convert_row(&row, &PixelFormat::bgrx32(), &mut out);
        assert_eq!(out, row);
    }

    #[test]
    fn rgb565_little_endian() {
        assert_eq!(convert(RED, &PixelFormat::rgb565()), [0x00, 0xf8]);
        // 128 scales to 16 of 31 and 32 of 63: 0x8410.
        assert_eq!(convert(GREY, &PixelFormat::rgb565()), [0x10, 0x84]);
        let be = PixelFormat {
            big_endian: true,
            ..PixelFormat::rgb565()
        };
        assert_eq!(convert(RED, &be), [0xf8, 0x00]);
    }

    #[test]
    fn bgr233_and_big_endian_rgbx() {
        assert_eq!(convert(RED, &PixelFormat::bgr233()), [0x07]);
        assert_eq!(convert([255, 0, 0, 0], &PixelFormat::bgr233()), [0xc0]);
        let rgbx_be = PixelFormat {
            big_endian: true,
            red_shift: 24,
            green_shift: 16,
            blue_shift: 8,
            ..PixelFormat::bgrx32()
        };
        assert_eq!(convert(RED, &rgbx_be), [255, 0, 0, 0]);
        assert_eq!(convert([0, 255, 0, 0], &rgbx_be), [0, 255, 0, 0]);
    }
}
