//! Pictures and pixel formats the encoder tests share.
//!
//! An encoder is only proved by a decoder, and a decoder only by a picture
//! that has something in it. These are the kinds of content the encodings
//! are built around: a flat fill, two colours the way text is two colours, a
//! handful, a lot, and noise that defeats all of them.
//!
//! A lossy pixel format cannot round-trip a picture exactly, so the tests
//! compare against [`through`], which is the same picture put through the
//! format and back with nothing else done to it. That is the most any
//! encoder could return.

use alloyvnc_proto::PixelFormat;

use crate::Framebuffer;
use crate::convert::convert_row;
use crate::decode::Unpacker;

/// A deterministic sequence, so a failure is the same failure tomorrow.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }

    pub fn below(&mut self, n: u32) -> u32 {
        self.next() % n.max(1)
    }
}

pub fn solid(width: u32, height: u32) -> Framebuffer {
    let mut fb = Framebuffer::new(width, height);
    fb.fill(fb.bounds(), [40, 80, 160, 0]);
    fb
}

/// Ink on a page: two colours, in shapes about the size of glyphs.
pub fn two_colour(width: u32, height: u32) -> Framebuffer {
    let mut fb = Framebuffer::new(width, height);
    let bounds = fb.bounds();
    fb.fill(bounds, [250, 250, 250, 0]);
    let mut rng = Rng(0x1234_5678_9abc_def0);
    for _ in 0..(width * height / 40).max(1) {
        let x = rng.below(width) as i32;
        let y = rng.below(height) as i32;
        let w = 1 + rng.below(4) as i32;
        let h = 1 + rng.below(8) as i32;
        fb.fill(alloyvnc_region::Rect::new(x, y, w, h), [20, 20, 20, 0]);
    }
    fb
}

/// Exactly `colours` distinct colours, in blocks rather than scattered, the
/// way a window with a few flat panels looks.
pub fn palette(width: u32, height: u32, colours: u32) -> Framebuffer {
    let mut fb = Framebuffer::new(width, height);
    let mut rng = Rng(0x0fed_cba9_8765_4321);
    let table: Vec<[u8; 4]> = (0..colours)
        .map(|i| {
            let i = i as u8;
            [i.wrapping_mul(37), i.wrapping_mul(59), i.wrapping_mul(97), 0]
        })
        .collect();
    for y in 0..height {
        for x in 0..width {
            // Blocks of four, so runs exist for the RLE paths to find.
            let cell = (x / 4 + y / 4 * 7) as usize;
            let jitter = if rng.below(16) == 0 { 1 } else { 0 };
            fb.put_pixel(x, y, table[(cell + jitter) % table.len()]);
        }
    }
    fb
}

/// Every pixel its own colour, which is what a photograph looks like to an
/// encoder and what every palette in here has to give up on.
pub fn noise(width: u32, height: u32) -> Framebuffer {
    let mut fb = Framebuffer::new(width, height);
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for y in 0..height {
        for x in 0..width {
            let v = rng.next();
            fb.put_pixel(x, y, [v as u8, (v >> 8) as u8, (v >> 16) as u8, 0]);
        }
    }
    fb
}

/// The formats an encoder has to survive: the framebuffer's own, what noVNC
/// asks for, two narrow ones, and one the wrong way round.
pub fn formats() -> Vec<(&'static str, PixelFormat)> {
    vec![
        ("bgrx32", PixelFormat::bgrx32()),
        (
            "rgbx32",
            PixelFormat {
                red_shift: 0,
                green_shift: 8,
                blue_shift: 16,
                ..PixelFormat::bgrx32()
            },
        ),
        ("rgb565", PixelFormat::rgb565()),
        ("bgr233", PixelFormat::bgr233()),
        (
            "bgrx32 big-endian",
            PixelFormat {
                big_endian: true,
                ..PixelFormat::bgrx32()
            },
        ),
    ]
}

/// The picture as the client will have it at best: through the format and
/// back, losing whatever the format cannot hold.
pub fn through(fb: &Framebuffer, pf: &PixelFormat) -> Framebuffer {
    let unpacker = Unpacker::new(pf);
    let bpp = unpacker.bytes_per_pixel();
    let mut out = Framebuffer::new(fb.width(), fb.height());
    let mut row = Vec::with_capacity(fb.width() as usize * bpp);
    for y in 0..fb.height() {
        row.clear();
        convert_row(fb.row(y), pf, &mut row);
        for (x, px) in row.chunks_exact(bpp).enumerate() {
            out.put_pixel(x as u32, y, unpacker.pixel(px));
        }
    }
    out
}

/// Where two pictures first differ, for a failure worth reading.
pub fn first_difference(a: &Framebuffer, b: &Framebuffer) -> Option<String> {
    if (a.width(), a.height()) != (b.width(), b.height()) {
        return Some(format!(
            "sizes differ: {}x{} against {}x{}",
            a.width(),
            a.height(),
            b.width(),
            b.height()
        ));
    }
    for y in 0..a.height() {
        for x in 0..a.width() {
            if a.pixel(x, y) != b.pixel(x, y) {
                let differing = (0..a.height())
                    .flat_map(|y| (0..a.width()).map(move |x| (x, y)))
                    .filter(|(x, y)| a.pixel(*x, *y) != b.pixel(*x, *y))
                    .count();
                return Some(format!(
                    "at {x},{y}: {:?} against {:?} ({differing} pixels differ of {})",
                    a.pixel(x, y),
                    b.pixel(x, y),
                    a.width() * a.height()
                ));
            }
        }
    }
    None
}
