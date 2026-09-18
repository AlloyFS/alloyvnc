//! Hextile (5): the rectangle in tiles, each described rather than sent.
//!
//! The oldest of the encodings worth having and the only one with no
//! compressor behind it, which is exactly what makes it useful: it costs
//! almost nothing in CPU and still turns a flat desktop into a few bytes a
//! tile. A tile of one colour is three bytes. A tile of two, which is what
//! text is, is its two colours and a list of little rectangles covering the
//! ink. Only a tile that is genuinely busy goes out as pixels.
//!
//! The two colours carry from tile to tile: a run of tiles sharing a
//! background says so once. That is the whole trick, and it is why the
//! encoder walks the tiles in order and keeps state while it does.
//!
//! Pixels are in the client's format throughout, so a tile is read out of
//! the framebuffer and converted once, then worked on as keys.

use alloyvnc_proto::PixelFormat;
use alloyvnc_region::Rect;

use crate::Framebuffer;
use crate::convert::convert_row;

/// Tiles are sixteen pixels on a side; the last row and column of a
/// rectangle are whatever is left.
const TILE: i32 = 16;

/// Past this many colours a tile is not worth describing, so counting
/// stops. Sixteen colours in two hundred and fifty-six pixels is already
/// busier than anything a palette would help with.
const TOO_MANY: usize = 24;

/// A tile may carry no more subrectangles than a byte can count.
const MAX_SUBRECTS: usize = 255;

/// The subencoding bits, as rfbproto names them.
pub mod flag {
    pub const RAW: u8 = 1;
    pub const BACKGROUND: u8 = 2;
    pub const FOREGROUND: u8 = 4;
    pub const ANY_SUBRECTS: u8 = 8;
    pub const SUBRECTS_COLOURED: u8 = 16;
}

/// One run of one colour inside a tile.
#[derive(Clone, Copy, Debug)]
struct Subrect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    colour: u32,
}

#[derive(Default)]
pub struct Hextile {
    /// One tile's pixels, converted, and the same pixels as keys to compare.
    bytes: Vec<u8>,
    keys: Vec<u32>,
    colours: Vec<(u32, usize)>,
    subrects: Vec<Subrect>,
    body: Vec<u8>,
    /// The tile `keys` holds, so the runs know where the rows end.
    width: i32,
    height: i32,
}

impl Hextile {
    pub fn new() -> Hextile {
        Hextile::default()
    }

    /// Append the rectangle's payload. The caller writes the rectangle
    /// header; this writes the tiles.
    pub fn encode(&mut self, fb: &Framebuffer, rect: Rect, pf: &PixelFormat, out: &mut Vec<u8>) {
        let bpp = pf.bytes_per_pixel();
        // The two colours persist across tiles, which is most of what makes
        // this encoding small. They start unknown, so the first tile that
        // wants one says what it is.
        let mut background: Option<u32> = None;
        let mut foreground: Option<u32> = None;

        let mut y = rect.y1;
        while y < rect.y2 {
            let height = TILE.min(rect.y2 - y);
            let mut x = rect.x1;
            while x < rect.x2 {
                let width = TILE.min(rect.x2 - x);
                let tile = Rect::new(x, y, width, height);
                self.read(fb, tile, pf, bpp);
                self.tile(bpp, &mut background, &mut foreground, out);
                x += TILE;
            }
            y += TILE;
        }
    }

    /// One tile's pixels out of the framebuffer, converted once.
    fn read(&mut self, fb: &Framebuffer, tile: Rect, pf: &PixelFormat, bpp: usize) {
        self.width = tile.width();
        self.height = tile.height();
        self.bytes.clear();
        for row in tile.y1..tile.y2 {
            let span = fb.row_span(row as u32, tile.x1 as u32, tile.width() as u32);
            convert_row(span, pf, &mut self.bytes);
        }
        self.keys.clear();
        self.keys.extend(self.bytes.chunks_exact(bpp).map(|px| {
            let mut word = [0u8; 4];
            word[..bpp].copy_from_slice(px);
            u32::from_le_bytes(word)
        }));
    }

    /// Encode the tile now in `keys`, which is `w` by `h`.
    fn tile(
        &mut self,
        bpp: usize,
        background: &mut Option<u32>,
        foreground: &mut Option<u32>,
        out: &mut Vec<u8>,
    ) {
        let raw_len = self.keys.len() * bpp;
        self.count_colours();

        // Nothing to describe: one colour, and it may even be the one the
        // last tile used, in which case the tile is a single byte.
        if self.colours.len() == 1 {
            let only = self.colours[0].0;
            let mut head = 0u8;
            if *background != Some(only) {
                head |= flag::BACKGROUND;
            }
            out.push(head);
            if head & flag::BACKGROUND != 0 {
                push_pixel(out, only, bpp);
                *background = Some(only);
            }
            return;
        }

        if self.colours.is_empty() {
            // More colours than are worth counting.
            self.raw(out);
            return;
        }

        // The commonest colour is the background, so the subrectangles
        // cover as little as possible.
        let (bg, _) = *self
            .colours
            .iter()
            .max_by_key(|(_, count)| *count)
            .expect("a tile has at least one colour");
        let coloured = self.colours.len() > 2;
        let fg = if coloured {
            None
        } else {
            self.colours.iter().map(|(c, _)| *c).find(|c| *c != bg)
        };
        self.runs(bg);
        if self.subrects.len() > MAX_SUBRECTS {
            self.raw(out);
            return;
        }

        // Build the tile, then keep it only if it beat sending the pixels.
        // The buffer comes out of the encoder and goes back at the end, so
        // a tile costs no allocation.
        let mut body = std::mem::take(&mut self.body);
        body.clear();
        let mut head = flag::ANY_SUBRECTS;
        let new_bg = *background != Some(bg);
        if new_bg {
            head |= flag::BACKGROUND;
        }
        let new_fg = match fg {
            Some(fg) => *foreground != Some(fg),
            None => false,
        };
        if new_fg {
            head |= flag::FOREGROUND;
        }
        if coloured {
            head |= flag::SUBRECTS_COLOURED;
        }
        if new_bg {
            push_pixel(&mut body, bg, bpp);
        }
        if let (true, Some(fg)) = (new_fg, fg) {
            push_pixel(&mut body, fg, bpp);
        }
        body.push(self.subrects.len() as u8);
        for sub in &self.subrects {
            if coloured {
                push_pixel(&mut body, sub.colour, bpp);
            }
            body.push(((sub.x as u8) << 4) | sub.y as u8);
            body.push((((sub.w - 1) as u8) << 4) | (sub.h - 1) as u8);
        }

        // Describing it has to actually be smaller than sending it.
        let worth_it = body.len() < raw_len;
        if worth_it {
            out.push(head);
            out.extend_from_slice(&body);
            if new_bg {
                *background = Some(bg);
            }
            if let (true, Some(fg)) = (new_fg, fg) {
                *foreground = Some(fg);
            }
        }
        self.body = body;
        if !worth_it {
            self.raw(out);
        }
    }

    fn raw(&mut self, out: &mut Vec<u8>) {
        out.push(flag::RAW);
        out.extend_from_slice(&self.bytes);
    }

    /// The tile's distinct colours with their counts, or empty when there
    /// are more than are worth describing.
    fn count_colours(&mut self) {
        self.colours.clear();
        for key in &self.keys {
            match self.colours.iter_mut().find(|(c, _)| c == key) {
                Some((_, count)) => *count += 1,
                None => {
                    if self.colours.len() == TOO_MANY {
                        self.colours.clear();
                        return;
                    }
                    self.colours.push((*key, 1));
                }
            }
        }
    }

    /// Every run of non-background pixels, merged downwards where the run
    /// above it is the same colour and the same span. A block of colour
    /// becomes one subrectangle rather than one per row.
    fn runs(&mut self, background: u32) {
        self.subrects.clear();
        let width = self.width;
        for y in 0..self.height {
            let row = &self.keys[(y * width) as usize..][..width as usize];
            let mut x = 0i32;
            while x < width {
                let colour = row[x as usize];
                if colour == background {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < width && row[x as usize] == colour {
                    x += 1;
                }
                let run = (start, x - start, colour);
                // The run directly above, if it matches, grows instead.
                let grown = self
                    .subrects
                    .iter_mut()
                    .rev()
                    .find(|s| s.x == run.0 && s.w == run.1 && s.colour == run.2 && s.y + s.h == y);
                match grown {
                    Some(above) => above.h += 1,
                    None => self.subrects.push(Subrect {
                        x: run.0,
                        y,
                        w: run.1,
                        h: 1,
                        colour: run.2,
                    }),
                }
            }
        }
    }
}

fn push_pixel(out: &mut Vec<u8>, key: u32, bpp: usize) {
    out.extend_from_slice(&key.to_le_bytes()[..bpp]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode;
    use crate::testing::{first_difference, formats, noise, palette, solid, through, two_colour};

    /// Encode, decode, and insist on the picture the format can hold.
    fn round_trip(fb: &Framebuffer, pf: &PixelFormat, what: &str) -> usize {
        let rect = fb.bounds();
        let mut payload = Vec::new();
        Hextile::new().encode(fb, rect, pf, &mut payload);
        let mut got = Framebuffer::new(fb.width(), fb.height());
        let used = decode::hextile(&payload, rect, pf, &mut got).unwrap_or_else(|e| panic!("{what}: {e}"));
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
                ("solid", solid(64, 48)),
                ("two colour", two_colour(64, 48)),
                ("sixteen colours", palette(64, 48, 16)),
                ("many colours", palette(64, 48, 200)),
                ("noise", noise(64, 48)),
            ] {
                round_trip(&fb, &pf, &format!("{kind} in {name}"));
            }
        }
    }

    #[test]
    fn sizes_that_do_not_divide_by_sixteen() {
        for (width, height) in [(1, 1), (1, 40), (40, 1), (17, 17), (31, 47), (16, 16), (100, 3)] {
            let fb = two_colour(width, height);
            for (name, pf) in formats() {
                round_trip(&fb, &pf, &format!("{width}x{height} in {name}"));
            }
        }
    }

    #[test]
    fn a_flat_tile_costs_almost_nothing() {
        let fb = solid(256, 256);
        let pf = PixelFormat::bgrx32();
        let bytes = round_trip(&fb, &pf, "solid");
        // Sixteen by sixteen tiles: the first says its colour, the rest say
        // nothing at all.
        assert_eq!(bytes, 1 + 4 + 255, "{bytes} bytes for a flat 256x256");
        assert!(bytes * 100 < fb.data().len(), "against {} raw", fb.data().len());
    }

    #[test]
    fn noise_falls_back_to_raw_rather_than_growing() {
        let fb = noise(64, 64);
        let pf = PixelFormat::bgrx32();
        let bytes = round_trip(&fb, &pf, "noise");
        // A tile it cannot describe costs its pixels and one byte.
        assert_eq!(bytes, fb.data().len() + 16, "{bytes} bytes for 64x64 of noise");
    }
}
