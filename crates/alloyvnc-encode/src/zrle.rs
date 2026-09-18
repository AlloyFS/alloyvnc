//! ZRLE (16): tiles of 64, described, then the whole rectangle deflated.
//!
//! Hextile's idea at a larger tile with a compressor behind it. Each tile is
//! turned into the smallest of a handful of descriptions, and the lot goes
//! through zlib, which finds the repetition between tiles that no single
//! tile can see.
//!
//! **The stream is session state, not a per-call helper.** A client's
//! inflater is built once and fed every rectangle in order, and its internal
//! window mirrors the server's deflater byte for byte. Compressing a
//! rectangle with a fresh deflater would produce bytes the client's inflater
//! cannot make sense of, and compressing two clients' rectangles through one
//! deflater would interleave two histories into one stream. One stream per
//! session, for the life of the session.
//!
//! **What a sync flush is for.** Deflate would rather hold bytes back and
//! wait for more input to compress them against. A sync flush says "emit
//! everything you are holding and end on a byte boundary", which is what
//! lets the client decode this rectangle without waiting for the next one.
//! It costs a few bytes of padding per rectangle and buys the picture
//! arriving when it was sent.

use alloyvnc_proto::PixelFormat;
use alloyvnc_region::Rect;
use flate2::{Compress, Compression, FlushCompress, Status};

use crate::Framebuffer;
use crate::convert::convert_row;

/// Tiles are sixty-four pixels on a side.
const TILE: i32 = 64;

/// The most colours a palette may hold before a tile is described some
/// other way. The subencoding byte carries `128 + size`, so this is what a
/// byte leaves room for.
const MAX_PALETTE: usize = 127;

/// Subencodings, as rfbproto numbers them.
pub mod sub {
    pub const RAW: u8 = 0;
    pub const SOLID: u8 = 1;
    /// 2 to 16: a packed palette of that many colours.
    pub const PACKED_MAX: u8 = 16;
    /// A packed palette reusing the tile before it.
    pub const PACKED_REUSE: u8 = 127;
    pub const PLAIN_RLE: u8 = 128;
    /// Palette RLE reusing the tile before it.
    pub const RLE_REUSE: u8 = 129;
    /// 130 to 255: palette RLE, the palette `sub - 128` long.
    pub const RLE_BASE: u8 = 128;
}

/// How a client's pixel goes into a ZRLE tile.
///
/// A 32-bit format whose colour all sits in three of its four bytes sends
/// only those three, which is a quarter off everything before zlib sees it.
/// Both the framebuffer's own format and the one noVNC asks for qualify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpixel {
    /// Bytes on the wire per pixel.
    pub bytes: usize,
    /// Where they start inside the full pixel.
    pub offset: usize,
    /// Bytes per pixel in the client's format.
    pub full: usize,
}

impl Cpixel {
    pub fn of(pf: &PixelFormat) -> Cpixel {
        let full = pf.bytes_per_pixel();
        let colour = (u32::from(pf.red_max) << pf.red_shift)
            | (u32::from(pf.green_max) << pf.green_shift)
            | (u32::from(pf.blue_max) << pf.blue_shift);
        if pf.bits_per_pixel == 32 && pf.depth <= 24 {
            // All of the colour in the low three bytes, or all of it in the
            // high three: either way one byte carries nothing and is left
            // behind. Which end of the wire that byte is at depends on the
            // format's byte order.
            if colour & 0xff00_0000 == 0 {
                return Cpixel {
                    bytes: 3,
                    offset: usize::from(pf.big_endian),
                    full,
                };
            }
            if colour & 0x0000_00ff == 0 {
                return Cpixel {
                    bytes: 3,
                    offset: usize::from(!pf.big_endian),
                    full,
                };
            }
        }
        Cpixel {
            bytes: full,
            offset: 0,
            full,
        }
    }
}

/// One run of one colour inside a tile, in reading order.
#[derive(Clone, Copy)]
struct Run {
    colour: u32,
    length: usize,
}

pub struct Zrle {
    stream: Compress,
    level: u32,
    /// The tiles of one rectangle before compression.
    plain: Vec<u8>,
    /// One tile, as keys and as bytes.
    keys: Vec<u32>,
    bytes: Vec<u8>,
    palette: Vec<u32>,
    runs: Vec<Run>,
}

impl Zrle {
    pub fn new(level: u32) -> Zrle {
        Zrle {
            stream: Compress::new(Compression::new(level), true),
            level,
            plain: Vec::new(),
            keys: Vec::new(),
            bytes: Vec::new(),
            palette: Vec::new(),
            runs: Vec::new(),
        }
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    /// Start the stream again, which a client has to be told about: it only
    /// happens when the session is resetting its own side too.
    pub fn reset(&mut self, level: u32) {
        self.level = level;
        self.stream = Compress::new(Compression::new(level), true);
    }

    /// Append the rectangle: a length, then the deflated tiles.
    pub fn encode(&mut self, fb: &Framebuffer, rect: Rect, pf: &PixelFormat, out: &mut Vec<u8>) {
        let cpixel = Cpixel::of(pf);
        self.plain.clear();
        let mut y = rect.y1;
        while y < rect.y2 {
            let height = TILE.min(rect.y2 - y);
            let mut x = rect.x1;
            while x < rect.x2 {
                let width = TILE.min(rect.x2 - x);
                self.read(fb, Rect::new(x, y, width, height), pf, cpixel);
                self.tile(width as usize, height as usize, cpixel);
                x += TILE;
            }
            y += TILE;
        }

        // The length goes in once the compressed size is known.
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        let plain = std::mem::take(&mut self.plain);
        deflate(&mut self.stream, &plain, out);
        self.plain = plain;
        let length = (out.len() - at - 4) as u32;
        out[at..at + 4].copy_from_slice(&length.to_be_bytes());
    }

    /// One tile's pixels, converted, as cpixel bytes and as keys.
    fn read(&mut self, fb: &Framebuffer, tile: Rect, pf: &PixelFormat, cpixel: Cpixel) {
        self.bytes.clear();
        for row in tile.y1..tile.y2 {
            let span = fb.row_span(row as u32, tile.x1 as u32, tile.width() as u32);
            convert_row(span, pf, &mut self.bytes);
        }
        self.keys.clear();
        self.keys.extend(self.bytes.chunks_exact(cpixel.full).map(|px| {
            let mut word = [0u8; 4];
            word[..cpixel.bytes].copy_from_slice(&px[cpixel.offset..cpixel.offset + cpixel.bytes]);
            u32::from_le_bytes(word)
        }));
    }

    /// Describe the tile in whichever way comes out smallest.
    fn tile(&mut self, width: usize, height: usize, cpixel: Cpixel) {
        let pixels = width * height;
        self.gather();

        if self.palette.len() == 1 {
            self.plain.push(sub::SOLID);
            push_cpixel(&mut self.plain, self.palette[0], cpixel);
            return;
        }

        // Every way this tile could go, costed, and the cheapest wins. The
        // alternative is a rule of thumb about run lengths; costing them is
        // a few additions and is never wrong.
        let raw = pixels * cpixel.bytes;
        let palette = self.palette.len();
        let packed = (2..=sub::PACKED_MAX as usize)
            .contains(&palette)
            .then(|| palette * cpixel.bytes + packed_rows(width, height, palette));
        let palette_rle = (2..=MAX_PALETTE).contains(&palette).then(|| {
            palette * cpixel.bytes
                + self
                    .runs
                    .iter()
                    .map(|r| 1 + if r.length > 1 { length_bytes(r.length) } else { 0 })
                    .sum::<usize>()
        });
        let plain_rle: usize = self
            .runs
            .iter()
            .map(|r| cpixel.bytes + length_bytes(r.length))
            .sum();

        let best = [Some(raw), packed, palette_rle, Some(plain_rle)]
            .into_iter()
            .enumerate()
            .filter_map(|(i, cost)| cost.map(|c| (c, i)))
            .min()
            .expect("raw is always available");
        match best.1 {
            1 => self.packed(width, height, cpixel),
            2 => self.palette_rle(cpixel),
            3 => self.plain_rle(cpixel),
            _ => {
                self.plain.push(sub::RAW);
                for key in &self.keys {
                    push_cpixel(&mut self.plain, *key, cpixel);
                }
            }
        }
    }

    /// The tile's colours, in first-seen order, and its runs. The palette is
    /// abandoned past the size a subencoding byte can name, but the runs are
    /// still wanted for plain RLE.
    fn gather(&mut self) {
        self.palette.clear();
        self.runs.clear();
        let mut over = false;
        for key in &self.keys {
            match self.runs.last_mut() {
                Some(run) if run.colour == *key => run.length += 1,
                _ => self.runs.push(Run {
                    colour: *key,
                    length: 1,
                }),
            }
            if !over && !self.palette.contains(key) {
                if self.palette.len() == MAX_PALETTE {
                    over = true;
                    self.palette.clear();
                } else {
                    self.palette.push(*key);
                }
            }
        }
    }

    fn index_of(&self, colour: u32) -> u8 {
        self.palette
            .iter()
            .position(|c| *c == colour)
            .expect("every colour of the tile is in its palette") as u8
    }

    fn packed(&mut self, width: usize, height: usize, cpixel: Cpixel) {
        self.plain.push(self.palette.len() as u8);
        for colour in &self.palette {
            push_cpixel(&mut self.plain, *colour, cpixel);
        }
        let bits = palette_bits(self.palette.len());
        for y in 0..height {
            // Each row starts on a byte, so a row of pixels never shares a
            // byte with the row below it.
            let mut byte = 0u8;
            let mut used = 0u32;
            for x in 0..width {
                let index = self.index_of(self.keys[y * width + x]);
                byte = (byte << bits) | index;
                used += bits;
                if used == 8 {
                    self.plain.push(byte);
                    byte = 0;
                    used = 0;
                }
            }
            if used > 0 {
                self.plain.push(byte << (8 - used));
            }
        }
    }

    fn palette_rle(&mut self, cpixel: Cpixel) {
        self.plain.push(sub::RLE_BASE + self.palette.len() as u8);
        for colour in &self.palette {
            push_cpixel(&mut self.plain, *colour, cpixel);
        }
        for i in 0..self.runs.len() {
            let run = self.runs[i];
            let index = self.index_of(run.colour);
            if run.length == 1 {
                self.plain.push(index);
            } else {
                // The top bit says a length follows.
                self.plain.push(index | 0x80);
                push_length(&mut self.plain, run.length);
            }
        }
    }

    fn plain_rle(&mut self, cpixel: Cpixel) {
        self.plain.push(sub::PLAIN_RLE);
        for i in 0..self.runs.len() {
            let run = self.runs[i];
            push_cpixel(&mut self.plain, run.colour, cpixel);
            push_length(&mut self.plain, run.length);
        }
    }
}

/// Bits per index for a palette of this size.
pub fn palette_bits(size: usize) -> u32 {
    match size {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    }
}

fn packed_rows(width: usize, height: usize, palette: usize) -> usize {
    let bits = palette_bits(palette) as usize;
    let per_row = (width * bits).div_ceil(8);
    per_row * height
}

/// A run length costs one byte per whole 255 and one for the remainder.
fn length_bytes(length: usize) -> usize {
    (length - 1) / 255 + 1
}

fn push_length(out: &mut Vec<u8>, length: usize) {
    let mut left = length - 1;
    while left >= 255 {
        out.push(255);
        left -= 255;
    }
    out.push(left as u8);
}

fn push_cpixel(out: &mut Vec<u8>, key: u32, cpixel: Cpixel) {
    out.extend_from_slice(&key.to_le_bytes()[..cpixel.bytes]);
}

/// Push `input` through the stream and append everything it gives back,
/// ending on a byte boundary so the client can read this rectangle now.
pub fn deflate(stream: &mut Compress, mut input: &[u8], out: &mut Vec<u8>) {
    loop {
        let consumed_before = stream.total_in();
        let produced_before = stream.total_out();
        // Deflate writes into whatever spare room the vector has, so there
        // has to be some before every call.
        out.reserve(4096);
        let status = stream
            .compress_vec(input, out, FlushCompress::Sync)
            .expect("deflate on a memory stream cannot fail");
        let consumed = (stream.total_in() - consumed_before) as usize;
        input = &input[consumed..];
        let produced = stream.total_out() - produced_before;
        if input.is_empty() && produced == 0 {
            break;
        }
        if status == Status::StreamEnd {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::ZrleReader;
    use crate::testing::{first_difference, formats, noise, palette, solid, through, two_colour};

    fn round_trip(fb: &Framebuffer, pf: &PixelFormat, what: &str) -> usize {
        let rect = fb.bounds();
        let mut payload = Vec::new();
        Zrle::new(1).encode(fb, rect, pf, &mut payload);
        let mut got = Framebuffer::new(fb.width(), fb.height());
        let used = ZrleReader::new()
            .decode(&payload, rect, pf, &mut got)
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(used, payload.len(), "{what}: trailing bytes");
        let want = through(fb, pf);
        if let Some(d) = first_difference(&want, &got) {
            panic!("{what}: {d}");
        }
        payload.len()
    }

    #[test]
    fn every_kind_of_picture_in_every_format() {
        for (name, pf) in formats() {
            for (kind, fb) in [
                ("solid", solid(96, 80)),
                ("two colour", two_colour(96, 80)),
                ("sixteen colours", palette(96, 80, 16)),
                ("a hundred colours", palette(96, 80, 100)),
                ("many colours", palette(96, 80, 300)),
                ("noise", noise(96, 80)),
            ] {
                round_trip(&fb, &pf, &format!("{kind} in {name}"));
            }
        }
    }

    #[test]
    fn sizes_that_do_not_divide_by_sixty_four() {
        for (width, height) in [(1, 1), (1, 70), (70, 1), (65, 65), (129, 33), (64, 64)] {
            let fb = palette(width, height, 8);
            for (name, pf) in formats() {
                round_trip(&fb, &pf, &format!("{width}x{height} in {name}"));
            }
        }
    }

    /// The point of the sync flush: one stream, rectangle after rectangle,
    /// each readable as it arrives.
    #[test]
    fn a_stream_carries_one_rectangle_after_another() {
        let pf = PixelFormat::bgrx32();
        let frames: Vec<Framebuffer> = (0..8).map(|i| palette(80, 48, 4 + i as u32 * 3)).collect();
        let mut encoder = Zrle::new(1);
        let mut reader = ZrleReader::new();
        let mut payload = Vec::new();
        for (i, fb) in frames.iter().enumerate() {
            payload.clear();
            let rect = fb.bounds();
            encoder.encode(fb, rect, &pf, &mut payload);
            let mut got = Framebuffer::new(fb.width(), fb.height());
            let used = reader
                .decode(&payload, rect, &pf, &mut got)
                .unwrap_or_else(|e| panic!("rectangle {i}: {e}"));
            assert_eq!(used, payload.len(), "rectangle {i}");
            if let Some(d) = first_difference(fb, &got) {
                panic!("rectangle {i}: {d}");
            }
        }
    }

    #[test]
    fn a_flat_rectangle_is_almost_nothing_and_noise_is_not_much_more_than_raw() {
        let pf = PixelFormat::bgrx32();
        let flat = solid(256, 256);
        let bytes = round_trip(&flat, &pf, "solid");
        assert!(bytes < 60, "{bytes} bytes for a flat 256x256");

        let busy = noise(256, 256);
        let raw = busy.data().len();
        let bytes = round_trip(&busy, &pf, "noise");
        // Three bytes a pixel rather than four, and deflate cannot help,
        // so it lands near three quarters of raw rather than above it.
        assert!(bytes < raw * 4 / 5, "{bytes} bytes against {raw} raw");
    }

    #[test]
    fn the_cpixel_drops_the_byte_nothing_uses() {
        assert_eq!(
            Cpixel::of(&PixelFormat::bgrx32()),
            Cpixel {
                bytes: 3,
                offset: 0,
                full: 4
            }
        );
        let rgbx = PixelFormat {
            red_shift: 0,
            green_shift: 8,
            blue_shift: 16,
            ..PixelFormat::bgrx32()
        };
        assert_eq!(
            Cpixel::of(&rgbx),
            Cpixel {
                bytes: 3,
                offset: 0,
                full: 4
            }
        );
        // Big-endian puts the unused byte first on the wire.
        let be = PixelFormat {
            big_endian: true,
            ..PixelFormat::bgrx32()
        };
        assert_eq!(
            Cpixel::of(&be),
            Cpixel {
                bytes: 3,
                offset: 1,
                full: 4
            }
        );
        // Sixteen bits has no spare byte to drop.
        assert_eq!(Cpixel::of(&PixelFormat::rgb565()).bytes, 2);
    }
}
