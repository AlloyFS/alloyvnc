//! Tight (7): the encoding a real viewer asks for.
//!
//! Where Hextile and ZRLE describe a fixed grid of tiles, Tight looks at a
//! whole rectangle and picks one method for it: a single colour is three
//! bytes, two colours become one bit a pixel, up to 256 become one byte a
//! pixel, and anything past that is either deflated as it stands or, when
//! the client said it would take one, a JPEG.
//!
//! **Why a palette beats deflate on text.** A line of black text on white is
//! two colours in a thousand pixels. Deflate has to discover that from the
//! bytes, and spends a match and a distance on every run it finds. A palette
//! says it once in the header and then spends one bit a pixel, and the
//! deflate that follows has an eighth as much to chew on. The same picture
//! goes from kilobytes to hundreds of bytes, and the encoder does less work
//! to get there.
//!
//! **The streams are session state.** Four of them, used by kind rather than
//! by rectangle, which is TigerVNC's convention: full-colour data on 0,
//! two-colour palettes on 1, larger palettes on 2. Keeping them apart means
//! each one's history is made of data that looks like itself. They live for
//! the session, because the client's four inflaters mirror them.

use alloyvnc_proto::PixelFormat;
use alloyvnc_region::Rect;
use flate2::{Compress, Compression};

use crate::Framebuffer;
use crate::convert::convert_row;
use crate::jpeg;
use crate::zrle::deflate;

/// No piece is wider than this, so a decoder never has to hold much.
const MAX_WIDTH: i32 = 2048;
/// Nor larger than this in pixels, which is what bounds a piece's buffers.
const MAX_PIXELS: i32 = 65536;
/// Below this many bytes, deflating costs more than it saves and the data
/// goes as it is with no length in front of it.
const MIN_TO_COMPRESS: usize = 12;
/// JPEG needs enough pixels to be worth its header and its blocks.
const MIN_FOR_JPEG: i32 = 4096;
/// A palette past this is not worth its indices.
const MAX_PALETTE: usize = 256;

/// The high nibble of the compression-control byte.
pub mod ctl {
    pub const FILL: u8 = 0x80;
    pub const JPEG: u8 = 0x90;
    /// Bit 6 of a Basic byte: a filter id follows.
    pub const EXPLICIT_FILTER: u8 = 0x40;
}

pub mod filter {
    pub const COPY: u8 = 0;
    pub const PALETTE: u8 = 1;
}

/// Which stream carries what, so each one's history stays uniform.
mod stream {
    pub const COPY: u8 = 0;
    pub const MONO: u8 = 1;
    pub const PALETTE: u8 = 2;
    pub const COUNT: usize = 4;
}

/// How a pixel goes into a Tight rectangle.
///
/// The common case throws away a byte and the client's shifts with it: a
/// 32-bit true-colour format at full range sends red, green and blue in that
/// order whatever order the client's own pixels are in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tpixel {
    pub three: bool,
    pub bytes: usize,
}

impl Tpixel {
    pub fn of(pf: &PixelFormat) -> Tpixel {
        let full = pf.bytes_per_pixel();
        let whole = (pf.red_max, pf.green_max, pf.blue_max) == (255, 255, 255);
        if pf.bits_per_pixel == 32 && pf.depth == 24 && whole {
            Tpixel {
                three: true,
                bytes: 3,
            }
        } else {
            Tpixel {
                three: false,
                bytes: full,
            }
        }
    }

    /// Append one framebuffer pixel.
    fn push(&self, bgrx: [u8; 4], pf: &PixelFormat, out: &mut Vec<u8>) {
        if self.three {
            out.extend_from_slice(&[bgrx[2], bgrx[1], bgrx[0]]);
        } else {
            convert_row(&bgrx, pf, out);
        }
    }
}

pub struct Tight {
    streams: Vec<Compress>,
    level: u32,
    /// Streams to tell the client to reset before the next piece, one bit
    /// each. Set after a level or format change, cleared once sent.
    pending_reset: u8,
    /// JPEG quality 0 to 9, or none, in which case nothing is ever lossy.
    quality: Option<u8>,
    /// One piece, as pixels and as keys to count colours with.
    pixels: Vec<u8>,
    keys: Vec<u32>,
    palette: Vec<u32>,
    body: Vec<u8>,
    rgb: Vec<u8>,
}

/// What a piece cost and how, so a session can say where its bytes went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    pub fill: u64,
    pub palette: u64,
    pub jpeg: u64,
    pub copy: u64,
}

impl Tight {
    pub fn new(level: u32, quality: Option<u8>) -> Tight {
        Tight {
            streams: (0..stream::COUNT)
                .map(|_| Compress::new(Compression::new(level), true))
                .collect(),
            level,
            // The client's streams start fresh too, so nothing has to be
            // reset before the first piece.
            pending_reset: 0,
            quality,
            pixels: Vec::new(),
            keys: Vec::new(),
            palette: Vec::new(),
            body: Vec::new(),
            rgb: Vec::new(),
        }
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    pub fn quality(&self) -> Option<u8> {
        self.quality
    }

    /// Start every stream again and tell the client to do the same. The
    /// reset bits ride on the next piece rather than going out on their own.
    pub fn reset(&mut self, level: u32, quality: Option<u8>) {
        self.level = level;
        self.quality = quality;
        for s in &mut self.streams {
            *s = Compress::new(Compression::new(level), true);
        }
        self.pending_reset = 0x0f;
    }

    /// Append the rectangle, splitting it into pieces a decoder can hold.
    pub fn encode(&mut self, fb: &Framebuffer, rect: Rect, pf: &PixelFormat, out: &mut Vec<u8>) -> Spent {
        let mut spent = Spent::default();
        // At most 2048 across, and then at most 65536 pixels, which for a
        // full-width piece is a band of rows.
        let rows = (MAX_PIXELS / rect.width().clamp(1, MAX_WIDTH)).max(1);
        let mut x = rect.x1;
        while x < rect.x2 {
            let width = MAX_WIDTH.min(rect.x2 - x);
            let mut y = rect.y1;
            while y < rect.y2 {
                let height = rows.min(rect.y2 - y);
                spent = spent.and(self.piece(fb, Rect::new(x, y, width, height), pf, out));
                y += height;
            }
            x += width;
        }
        spent
    }

    /// How many pieces a rectangle becomes, which the session needs for the
    /// rectangle count in the update header.
    pub fn pieces(rect: Rect) -> usize {
        let rows = (MAX_PIXELS / rect.width().clamp(1, MAX_WIDTH)).max(1);
        let across = (rect.width().max(1) as u32).div_ceil(MAX_WIDTH as u32);
        let down = (rect.height().max(1) as u32).div_ceil(rows as u32);
        (across * down) as usize
    }

    /// The rectangles a session should write headers for, in order.
    pub fn piece_rects(rect: Rect) -> Vec<Rect> {
        let rows = (MAX_PIXELS / rect.width().clamp(1, MAX_WIDTH)).max(1);
        let mut out = Vec::new();
        let mut x = rect.x1;
        while x < rect.x2 {
            let width = MAX_WIDTH.min(rect.x2 - x);
            let mut y = rect.y1;
            while y < rect.y2 {
                let height = rows.min(rect.y2 - y);
                out.push(Rect::new(x, y, width, height));
                y += height;
            }
            x += width;
        }
        out
    }

    fn piece(&mut self, fb: &Framebuffer, piece: Rect, pf: &PixelFormat, out: &mut Vec<u8>) -> Spent {
        let tpixel = Tpixel::of(pf);
        self.read(fb, piece, pf, tpixel);
        let colours = self.count_colours();
        let at = out.len();
        let mut spent = Spent::default();

        match colours {
            1 => {
                out.push(ctl::FILL | self.take_reset());
                push_key(out, self.palette[0], tpixel.bytes);
                spent.fill = (out.len() - at) as u64;
            }
            2 => {
                self.mono(tpixel, piece, out);
                spent.palette = (out.len() - at) as u64;
            }
            n if n > 0 && n <= MAX_PALETTE => {
                self.indexed(tpixel, out);
                spent.palette = (out.len() - at) as u64;
            }
            _ => {
                let big = piece.area() >= i64::from(MIN_FOR_JPEG);
                match self.quality.filter(|_| big) {
                    Some(level) => {
                        self.jpeg(fb, piece, level, out);
                        spent.jpeg = (out.len() - at) as u64;
                    }
                    None => {
                        self.copy(out);
                        spent.copy = (out.len() - at) as u64;
                    }
                }
            }
        }
        spent
    }

    /// The piece's pixels as tpixel bytes, and as keys to compare.
    fn read(&mut self, fb: &Framebuffer, piece: Rect, pf: &PixelFormat, tpixel: Tpixel) {
        self.pixels.clear();
        for y in piece.y1..piece.y2 {
            for x in piece.x1..piece.x2 {
                tpixel.push(fb.pixel(x as u32, y as u32), pf, &mut self.pixels);
            }
        }
        self.keys.clear();
        self.keys.extend(self.pixels.chunks_exact(tpixel.bytes).map(|px| {
            let mut word = [0u8; 4];
            word[..tpixel.bytes.min(4)].copy_from_slice(&px[..tpixel.bytes.min(4)]);
            u32::from_le_bytes(word)
        }));
    }

    /// The distinct colours, or zero when there are more than a palette holds.
    fn count_colours(&mut self) -> usize {
        self.palette.clear();
        for key in &self.keys {
            if !self.palette.contains(key) {
                if self.palette.len() == MAX_PALETTE {
                    self.palette.clear();
                    return 0;
                }
                self.palette.push(*key);
            }
        }
        self.palette.len()
    }

    fn take_reset(&mut self) -> u8 {
        std::mem::take(&mut self.pending_reset)
    }

    /// Two colours, one bit a pixel, rows padded to a byte.
    fn mono(&mut self, tpixel: Tpixel, piece: Rect, out: &mut Vec<u8>) {
        let mut body = std::mem::take(&mut self.body);
        body.clear();
        let width = piece.width() as usize;
        for row in self.keys.chunks(width) {
            let mut byte = 0u8;
            let mut used = 0;
            for key in row {
                // The most significant bit first, which is the order
                // rfbproto gives for the mono filter.
                byte = (byte << 1) | u8::from(*key == self.palette[1]);
                used += 1;
                if used == 8 {
                    body.push(byte);
                    byte = 0;
                    used = 0;
                }
            }
            if used > 0 {
                body.push(byte << (8 - used));
            }
        }
        self.basic(stream::MONO, Some(filter::PALETTE), tpixel, 2, &body, out);
        self.body = body;
    }

    /// Up to 256 colours, one byte a pixel.
    fn indexed(&mut self, tpixel: Tpixel, out: &mut Vec<u8>) {
        let mut body = std::mem::take(&mut self.body);
        body.clear();
        body.extend(self.keys.iter().map(|key| {
            self.palette
                .iter()
                .position(|c| c == key)
                .expect("every colour is in the palette") as u8
        }));
        let size = self.palette.len();
        self.basic(stream::PALETTE, Some(filter::PALETTE), tpixel, size, &body, out);
        self.body = body;
    }

    /// The pixels as they are, deflated.
    fn copy(&mut self, out: &mut Vec<u8>) {
        let pixels = std::mem::take(&mut self.pixels);
        let head = ctl::EXPLICIT_FILTER | (u32::from(stream::COPY) << 4) as u8 | self.take_reset();
        out.push(head);
        out.push(filter::COPY);
        self.compressed(stream::COPY, &pixels, out);
        self.pixels = pixels;
    }

    /// A Basic piece: the control byte, the filter, the palette if there is
    /// one, then the data.
    fn basic(
        &mut self,
        id: u8,
        filter_id: Option<u8>,
        tpixel: Tpixel,
        palette: usize,
        body: &[u8],
        out: &mut Vec<u8>,
    ) {
        let mut head = (id << 4) | self.take_reset();
        if filter_id.is_some() {
            head |= ctl::EXPLICIT_FILTER;
        }
        out.push(head);
        if let Some(filter_id) = filter_id {
            out.push(filter_id);
            if filter_id == filter::PALETTE {
                out.push((palette - 1) as u8);
                for i in 0..palette {
                    push_key(out, self.palette[i], tpixel.bytes);
                }
            }
        }
        self.compressed(id, body, out);
    }

    /// Deflate on the piece's own stream, with a length in front, unless it
    /// is too short to be worth either.
    fn compressed(&mut self, id: u8, body: &[u8], out: &mut Vec<u8>) {
        if body.len() < MIN_TO_COMPRESS {
            out.extend_from_slice(body);
            return;
        }
        let mut packed = Vec::with_capacity(body.len() / 2 + 64);
        deflate(&mut self.streams[usize::from(id)], body, &mut packed);
        push_compact(out, packed.len());
        out.extend_from_slice(&packed);
    }

    fn jpeg(&mut self, fb: &Framebuffer, piece: Rect, level: u8, out: &mut Vec<u8>) {
        self.rgb.clear();
        for y in piece.y1..piece.y2 {
            for x in piece.x1..piece.x2 {
                let px = fb.pixel(x as u32, y as u32);
                self.rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        match jpeg::encode(&self.rgb, piece.width() as u16, piece.height() as u16, level) {
            Ok(data) => {
                out.push(ctl::JPEG | self.take_reset());
                push_compact(out, data.len());
                out.extend_from_slice(&data);
            }
            Err(e) => {
                // Nothing here is worth dropping a frame over: the piece
                // goes out whole instead.
                tracing_warn(&e);
                self.copy(out);
            }
        }
    }
}

impl Spent {
    fn and(self, other: Spent) -> Spent {
        Spent {
            fill: self.fill + other.fill,
            palette: self.palette + other.palette,
            jpeg: self.jpeg + other.jpeg,
            copy: self.copy + other.copy,
        }
    }

    pub fn total(&self) -> u64 {
        self.fill + self.palette + self.jpeg + self.copy
    }
}

/// The encode crate has no logger of its own, and one failure worth one line
/// is not a reason to give it one.
fn tracing_warn(message: &str) {
    eprintln!("alloyvnc-encode: {message}");
}

fn push_key(out: &mut Vec<u8>, key: u32, bytes: usize) {
    out.extend_from_slice(&key.to_le_bytes()[..bytes.min(4)]);
}

/// A length in one to three bytes, seven bits each, low group first, the
/// high bit saying another follows.
pub fn push_compact(out: &mut Vec<u8>, mut length: usize) {
    out.push((length & 0x7f) as u8 | if length > 0x7f { 0x80 } else { 0 });
    if length > 0x7f {
        length >>= 7;
        out.push((length & 0x7f) as u8 | if length > 0x7f { 0x80 } else { 0 });
        if length > 0x7f {
            out.push(((length >> 7) & 0xff) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::TightReader;
    use crate::testing::{first_difference, formats, noise, palette, solid, through, two_colour};

    /// Encode piece by piece, the way a session does, and decode the same.
    fn round_trip(fb: &Framebuffer, pf: &PixelFormat, quality: Option<u8>, what: &str) -> usize {
        let rect = fb.bounds();
        let mut encoder = Tight::new(1, quality);
        let mut reader = TightReader::new();
        let mut got = Framebuffer::new(fb.width(), fb.height());
        let mut total = 0;
        for piece in Tight::piece_rects(rect) {
            let mut payload = Vec::new();
            encoder.encode(fb, piece, pf, &mut payload);
            let used = reader
                .decode(&payload, piece, pf, &mut got)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(used, payload.len(), "{what}: trailing bytes");
            total += payload.len();
        }
        if quality.is_none() {
            let want = through(fb, pf);
            if let Some(d) = first_difference(&want, &got) {
                panic!("{what}: {d}");
            }
        }
        total
    }

    #[test]
    fn every_kind_of_picture_in_every_format() {
        for (name, pf) in formats() {
            for (kind, fb) in [
                ("solid", solid(80, 60)),
                ("two colour", two_colour(80, 60)),
                ("sixteen colours", palette(80, 60, 16)),
                ("two hundred colours", palette(80, 60, 200)),
                ("noise", noise(80, 60)),
            ] {
                round_trip(&fb, &pf, None, &format!("{kind} in {name}"));
            }
        }
    }

    #[test]
    fn odd_sizes_and_a_rectangle_too_wide_for_one_piece() {
        for (width, height) in [(1, 1), (1, 33), (33, 1), (17, 19), (2048, 3)] {
            let fb = palette(width, height, 6);
            round_trip(&fb, &PixelFormat::bgrx32(), None, &format!("{width}x{height}"));
        }
        // Past 2048 the rectangle becomes more than one piece, and the
        // pieces have to line up or the picture comes back shifted.
        let wide = palette(2500, 8, 12);
        assert_eq!(Tight::pieces(wide.bounds()), 2);
        round_trip(&wide, &PixelFormat::bgrx32(), None, "2500 wide");
        // And a piece is bounded by pixels as well as by width.
        let tall = two_colour(2048, 64);
        assert!(Tight::pieces(tall.bounds()) > 1, "65536 pixels a piece");
        round_trip(&tall, &PixelFormat::bgrx32(), None, "2048 by 64");
    }

    #[test]
    fn each_kind_of_picture_goes_the_way_it_should() {
        let pf = PixelFormat::bgrx32();

        // One colour is a fill, whatever the size.
        let flat = solid(200, 200);
        let bytes = round_trip(&flat, &pf, None, "solid");
        assert_eq!(bytes, 4, "a fill is a control byte and a pixel, not {bytes}");

        // Two colours are a bit a pixel before deflate even starts.
        let text = two_colour(200, 200);
        let bytes = round_trip(&text, &pf, None, "text");
        assert!(
            bytes < text.data().len() / 40,
            "{bytes} bytes for text against {} raw",
            text.data().len()
        );

        // Noise has nothing to find, so it is the pixels and a little more.
        let busy = noise(200, 200);
        let bytes = round_trip(&busy, &pf, None, "noise");
        let three_quarters = busy.data().len() * 3 / 4;
        assert!(
            bytes < three_quarters + three_quarters / 10,
            "{bytes} bytes against {three_quarters} of tpixels"
        );
    }

    #[test]
    fn a_quality_level_turns_the_busy_parts_into_jpeg() {
        let pf = PixelFormat::bgrx32();
        // A gradient: no palette can hold it and deflate cannot help.
        let mut fb = Framebuffer::new(128, 128);
        for y in 0..128u32 {
            for x in 0..128u32 {
                fb.put_pixel(x, y, [(x * 2) as u8, (y * 2) as u8, (x + y) as u8, 0]);
            }
        }
        let lossless = round_trip(&fb, &pf, None, "gradient lossless");
        let lossy = round_trip(&fb, &pf, Some(6), "gradient at quality six");
        assert!(lossy * 4 < lossless, "{lossy} against {lossless} lossless");

        // And the picture that comes back is the picture, within reason.
        let mut encoder = Tight::new(1, Some(6));
        let mut reader = TightReader::new();
        let mut got = Framebuffer::new(fb.width(), fb.height());
        let mut payload = Vec::new();
        let rect = fb.bounds();
        let spent = encoder.encode(&fb, rect, &pf, &mut payload);
        reader.decode(&payload, rect, &pf, &mut got).expect("decodes");
        assert!(spent.jpeg > 0, "it went as JPEG: {spent:?}");
        let worst = (0..fb.height())
            .flat_map(|y| (0..fb.width()).map(move |x| (x, y)))
            .map(|(x, y)| {
                let (a, b) = (fb.pixel(x, y), got.pixel(x, y));
                (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        assert!(worst <= 24, "the worst channel is {worst} off");
    }

    #[test]
    fn a_small_piece_carries_its_data_without_a_length() {
        // Under twelve bytes the data goes as it is, so a decoder that
        // expected a length would read the pixels as one.
        let fb = palette(2, 2, 4);
        round_trip(&fb, &PixelFormat::bgrx32(), None, "four pixels");
    }

    #[test]
    fn a_reset_is_announced_once_and_then_forgotten() {
        let pf = PixelFormat::bgrx32();
        let fb = two_colour(64, 64);
        let mut encoder = Tight::new(1, None);
        let mut payload = Vec::new();
        encoder.encode(&fb, fb.bounds(), &pf, &mut payload);
        assert_eq!(payload[0] & 0x0f, 0, "nothing to reset on a new session");

        encoder.reset(6, None);
        payload.clear();
        encoder.encode(&fb, fb.bounds(), &pf, &mut payload);
        assert_eq!(payload[0] & 0x0f, 0x0f, "every stream, once");
        payload.clear();
        encoder.encode(&fb, fb.bounds(), &pf, &mut payload);
        assert_eq!(payload[0] & 0x0f, 0, "and not again");
    }

    #[test]
    fn a_compact_length_round_trips() {
        for length in [0usize, 1, 127, 128, 255, 16383, 16384, 100_000] {
            let mut out = Vec::new();
            push_compact(&mut out, length);
            assert!(out.len() <= 3, "{length} took {} bytes", out.len());
            // Read it back the way the decoder does.
            let mut got = 0usize;
            for (group, byte) in out.iter().enumerate() {
                got |= usize::from(byte & 0x7f) << (group * 7);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            assert_eq!(got, length);
        }
    }
}
