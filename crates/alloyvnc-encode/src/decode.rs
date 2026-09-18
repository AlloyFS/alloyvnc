//! The encoders run backwards.
//!
//! Every encoder here has a decoder beside it, and not only so the tests can
//! check a round trip, though that is what they are for first: an encoder
//! tested against its own idea of the format proves nothing, and a decoder
//! written from the same paragraph of rfbproto at least fails differently.
//! The test client uses these, and a recorder will.
//!
//! A decoder writes into a [`Framebuffer`], which is BGRX, so it has to
//! undo the client's pixel format on the way in. [`Unpacker`] is the inverse
//! of [`Packer`](crate::convert::Packer), which is what lets a test run any
//! true-colour format through an encoder and get its pixels back.

use alloyvnc_proto::PixelFormat;
use alloyvnc_region::Rect;
use flate2::{Decompress, FlushDecompress, Status};

use crate::Framebuffer;
use crate::hextile::flag;
use crate::tight::{Tpixel, ctl, filter};
use crate::zrle::{Cpixel, palette_bits, sub};

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("{0}")]
    Malformed(String),
    /// The payload stops in the middle of something. A reader taking bytes
    /// off a socket can answer this by fetching more; a corrupt stream
    /// cannot, which is why it is a variant of its own.
    #[error("the payload ends early: {0} more bytes wanted")]
    Truncated(usize),
}

type Result<T> = std::result::Result<T, DecodeError>;

fn bad(what: impl Into<String>) -> DecodeError {
    DecodeError::Malformed(what.into())
}

/// Turns a client's pixels back into the framebuffer's.
#[derive(Clone, Copy, Debug)]
pub struct Unpacker {
    bytes: usize,
    big_endian: bool,
    red: (u32, u32),
    green: (u32, u32),
    blue: (u32, u32),
}

impl Unpacker {
    pub fn new(pf: &PixelFormat) -> Unpacker {
        Unpacker {
            bytes: pf.bytes_per_pixel(),
            big_endian: pf.big_endian,
            red: (u32::from(pf.red_shift), u32::from(pf.red_max)),
            green: (u32::from(pf.green_shift), u32::from(pf.green_max)),
            blue: (u32::from(pf.blue_shift), u32::from(pf.blue_max)),
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        self.bytes
    }

    /// One pixel of the client's format as the framebuffer's BGRX.
    pub fn pixel(&self, src: &[u8]) -> [u8; 4] {
        let mut word = [0u8; 4];
        if self.big_endian {
            // The high byte came first, so it lands at the top of the word.
            for (i, b) in src[..self.bytes].iter().enumerate() {
                word[self.bytes - 1 - i] = *b;
            }
        } else {
            word[..self.bytes].copy_from_slice(&src[..self.bytes]);
        }
        let value = u32::from_le_bytes(word);
        [
            channel(value, self.blue),
            channel(value, self.green),
            channel(value, self.red),
            0,
        ]
    }
}

/// One channel out of a packed pixel, widened back to eight bits. The
/// rounding matches the packer's, so a format that can hold eight bits
/// round-trips exactly and a narrower one lands on the nearest value it can.
fn channel(value: u32, (shift, max): (u32, u32)) -> u8 {
    if max == 0 {
        return 0;
    }
    let raw = (value >> shift) & max;
    if max == 255 {
        raw as u8
    } else {
        ((raw * 255 + max / 2) / max) as u8
    }
}

/// A cursor over a rectangle's payload.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self.data.get(self.at).ok_or(DecodeError::Truncated(1))?;
        self.at += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| bad("length overflow"))?;
        let slice = self
            .data
            .get(self.at..end)
            .ok_or_else(|| DecodeError::Truncated(end.saturating_sub(self.data.len())))?;
        self.at = end;
        Ok(slice)
    }
}

/// Hextile (5) into `fb`. Returns the bytes consumed.
pub fn hextile(payload: &[u8], rect: Rect, pf: &PixelFormat, fb: &mut Framebuffer) -> Result<usize> {
    let unpacker = Unpacker::new(pf);
    let bpp = unpacker.bytes_per_pixel();
    let mut reader = Reader::new(payload);
    let mut background = [0u8; 4];
    let mut foreground = [0u8; 4];

    let mut y = rect.y1;
    while y < rect.y2 {
        let height = 16.min(rect.y2 - y);
        let mut x = rect.x1;
        while x < rect.x2 {
            let width = 16.min(rect.x2 - x);
            let head = reader.byte()?;
            if head & flag::RAW != 0 {
                let pixels = reader.take(width as usize * height as usize * bpp)?;
                for (i, px) in pixels.chunks_exact(bpp).enumerate() {
                    let (dx, dy) = (i as i32 % width, i as i32 / width);
                    fb.put_pixel((x + dx) as u32, (y + dy) as u32, unpacker.pixel(px));
                }
                x += 16;
                continue;
            }
            if head & flag::BACKGROUND != 0 {
                background = unpacker.pixel(reader.take(bpp)?);
            }
            if head & flag::FOREGROUND != 0 {
                foreground = unpacker.pixel(reader.take(bpp)?);
            }
            fb.fill(Rect::new(x, y, width, height), background);
            if head & flag::ANY_SUBRECTS == 0 {
                x += 16;
                continue;
            }
            let count = reader.byte()?;
            let coloured = head & flag::SUBRECTS_COLOURED != 0;
            for _ in 0..count {
                let colour = if coloured {
                    unpacker.pixel(reader.take(bpp)?)
                } else {
                    foreground
                };
                let position = reader.byte()?;
                let size = reader.byte()?;
                let (sx, sy) = (i32::from(position >> 4), i32::from(position & 0x0f));
                let (sw, sh) = (i32::from(size >> 4) + 1, i32::from(size & 0x0f) + 1);
                if sx + sw > width || sy + sh > height {
                    return Err(bad(format!(
                        "subrect {sx},{sy} {sw}x{sh} outside a {width}x{height} tile"
                    )));
                }
                fb.fill(Rect::new(x + sx, y + sy, sw, sh), colour);
            }
            x += 16;
        }
        y += 16;
    }
    Ok(reader.at)
}

/// ZRLE (16) into `fb`.
///
/// The stream is state, the same way the encoder's is: one inflater per
/// session, fed every rectangle in order. A decoder that made a fresh one
/// per rectangle would work only for the first.
pub struct ZrleReader {
    stream: Decompress,
    plain: Vec<u8>,
    palette: Vec<[u8; 4]>,
}

impl Default for ZrleReader {
    fn default() -> ZrleReader {
        ZrleReader::new()
    }
}

impl ZrleReader {
    pub fn new() -> ZrleReader {
        ZrleReader {
            stream: Decompress::new(true),
            plain: Vec::new(),
            palette: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.stream = Decompress::new(true);
    }

    /// Returns the bytes of `payload` consumed: the length prefix and the
    /// compressed data it names.
    pub fn decode(
        &mut self,
        payload: &[u8],
        rect: Rect,
        pf: &PixelFormat,
        fb: &mut Framebuffer,
    ) -> Result<usize> {
        let head: [u8; 4] = payload
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(DecodeError::Truncated(4))?;
        let length = u32::from_be_bytes(head) as usize;
        let body = payload
            .get(4..4 + length)
            .ok_or_else(|| DecodeError::Truncated((4 + length).saturating_sub(payload.len())))?;

        self.plain.clear();
        let mut plain = std::mem::take(&mut self.plain);
        inflate(&mut self.stream, body, &mut plain)?;
        let cpixel = Cpixel::of(pf);
        let unpacker = Unpacker::new(pf);
        {
            let mut reader = Reader::new(&plain);
            let mut y = rect.y1;
            while y < rect.y2 {
                let height = 64.min(rect.y2 - y);
                let mut x = rect.x1;
                while x < rect.x2 {
                    let width = 64.min(rect.x2 - x);
                    let tile = Rect::new(x, y, width, height);
                    self.tile(&mut reader, tile, cpixel, &unpacker, fb)?;
                    x += 64;
                }
                y += 64;
            }
        }
        self.plain = plain;
        Ok(4 + length)
    }

    fn tile(
        &mut self,
        reader: &mut Reader<'_>,
        tile: Rect,
        cpixel: Cpixel,
        unpacker: &Unpacker,
        fb: &mut Framebuffer,
    ) -> Result<()> {
        let (width, height) = (tile.width(), tile.height());
        let pixels = (width * height) as usize;
        let subencoding = reader.byte()?;

        match subencoding {
            sub::RAW => {
                for i in 0..pixels {
                    let px = read_cpixel(reader, cpixel, unpacker)?;
                    put(fb, tile, i, width, px);
                }
            }
            sub::SOLID => {
                let px = read_cpixel(reader, cpixel, unpacker)?;
                fb.fill(tile, px);
            }
            2..=sub::PACKED_MAX | sub::PACKED_REUSE => {
                if subencoding != sub::PACKED_REUSE {
                    self.palette = read_palette(reader, usize::from(subencoding), cpixel, unpacker)?;
                }
                let bits = palette_bits(self.palette.len());
                for row in 0..height {
                    // Every row starts on a byte of its own.
                    let per_row = (width as u32 * bits).div_ceil(8) as usize;
                    let packed = reader.take(per_row)?;
                    for column in 0..width {
                        let at = column as u32 * bits;
                        let byte = packed[(at / 8) as usize];
                        let shift = 8 - bits - (at % 8);
                        let mask = ((1u32 << bits) - 1) as u8;
                        let index = usize::from((byte >> shift) & mask);
                        let px = *self
                            .palette
                            .get(index)
                            .ok_or_else(|| bad(format!("palette index {index} of {}", self.palette.len())))?;
                        fb.put_pixel((tile.x1 + column) as u32, (tile.y1 + row) as u32, px);
                    }
                }
            }
            sub::PLAIN_RLE => {
                let mut at = 0usize;
                while at < pixels {
                    let px = read_cpixel(reader, cpixel, unpacker)?;
                    let run = read_length(reader)?;
                    for _ in 0..run.min(pixels - at) {
                        put(fb, tile, at, width, px);
                        at += 1;
                    }
                }
            }
            sub::RLE_REUSE | 130..=255 => {
                if subencoding != sub::RLE_REUSE {
                    let size = usize::from(subencoding - sub::RLE_BASE);
                    self.palette = read_palette(reader, size, cpixel, unpacker)?;
                }
                let mut at = 0usize;
                while at < pixels {
                    let index = reader.byte()?;
                    let run = if index & 0x80 != 0 {
                        read_length(reader)?
                    } else {
                        1
                    };
                    let px = *self.palette.get(usize::from(index & 0x7f)).ok_or_else(|| {
                        bad(format!(
                            "palette index {} of {}",
                            index & 0x7f,
                            self.palette.len()
                        ))
                    })?;
                    for _ in 0..run.min(pixels - at) {
                        put(fb, tile, at, width, px);
                        at += 1;
                    }
                }
            }
            other => return Err(bad(format!("ZRLE subencoding {other}"))),
        }
        Ok(())
    }
}

fn read_palette(
    reader: &mut Reader<'_>,
    size: usize,
    cpixel: Cpixel,
    unpacker: &Unpacker,
) -> Result<Vec<[u8; 4]>> {
    (0..size).map(|_| read_cpixel(reader, cpixel, unpacker)).collect()
}

fn put(fb: &mut Framebuffer, tile: Rect, index: usize, width: i32, px: [u8; 4]) {
    let (dx, dy) = (index as i32 % width, index as i32 / width);
    fb.put_pixel((tile.x1 + dx) as u32, (tile.y1 + dy) as u32, px);
}

fn read_cpixel(reader: &mut Reader<'_>, cpixel: Cpixel, unpacker: &Unpacker) -> Result<[u8; 4]> {
    let bytes = reader.take(cpixel.bytes)?;
    // The byte the format does not use goes back as zero, which is what the
    // encoder left out.
    let mut full = [0u8; 4];
    full[cpixel.offset..cpixel.offset + cpixel.bytes].copy_from_slice(bytes);
    Ok(unpacker.pixel(&full[..cpixel.full]))
}

/// A run length: bytes of 255 that add up, then one below 255, plus one.
fn read_length(reader: &mut Reader<'_>) -> Result<usize> {
    let mut total = 1usize;
    loop {
        let byte = reader.byte()?;
        total += usize::from(byte);
        if byte != 255 {
            return Ok(total);
        }
    }
}

/// Pull everything the stream will give back for `input`.
fn inflate(stream: &mut Decompress, mut input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    loop {
        let consumed_before = stream.total_in();
        let produced_before = stream.total_out();
        out.reserve(4096);
        let status = stream
            .decompress_vec(input, out, FlushDecompress::Sync)
            .map_err(|e| bad(format!("inflate: {e}")))?;
        let consumed = (stream.total_in() - consumed_before) as usize;
        input = &input[consumed..];
        let produced = stream.total_out() - produced_before;
        if input.is_empty() && produced == 0 {
            return Ok(());
        }
        if status == Status::StreamEnd {
            return Ok(());
        }
    }
}

/// Tight (7) into `fb`, one piece per call.
///
/// A Tight rectangle on the wire is one piece, so the session writes a
/// rectangle header for each and this reads one. The four streams are
/// session state, like ZRLE's one, and the control byte says when the server
/// has started any of them again.
pub struct TightReader {
    streams: Vec<Decompress>,
    scratch: Vec<u8>,
}

impl Default for TightReader {
    fn default() -> TightReader {
        TightReader::new()
    }
}

impl TightReader {
    pub fn new() -> TightReader {
        TightReader {
            streams: (0..4).map(|_| Decompress::new(true)).collect(),
            scratch: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        for s in &mut self.streams {
            *s = Decompress::new(true);
        }
    }

    /// How long this piece is, from its header alone.
    ///
    /// A Tight rectangle describes its own length, but only after the fact:
    /// the control byte says which shape it is and the shape says how much
    /// follows. A reader pulling bytes off a socket has to know before it
    /// decodes, because feeding a zlib stream half a block ruins it for
    /// every rectangle after. This parses the header and nothing else, so it
    /// is safe to call on a payload that is still arriving.
    pub fn payload_len(payload: &[u8], rect: Rect, pf: &PixelFormat) -> Result<usize> {
        let tpixel = Tpixel::of(pf);
        let mut reader = Reader::new(payload);
        let control = reader.byte()?;
        let pixels = (rect.width() * rect.height()) as usize;
        match control & 0xf0 {
            ctl::FILL => {
                reader.take(tpixel.bytes)?;
            }
            ctl::JPEG => {
                let length = read_compact(&mut reader)?;
                reader.take(length)?;
            }
            _ => {
                let filter_id = if control & ctl::EXPLICIT_FILTER != 0 {
                    reader.byte()?
                } else {
                    filter::COPY
                };
                let want = match filter_id {
                    filter::PALETTE => {
                        let size = usize::from(reader.byte()?) + 1;
                        reader.take(size * tpixel.bytes)?;
                        if size == 2 {
                            (rect.width() as usize).div_ceil(8) * rect.height() as usize
                        } else {
                            pixels
                        }
                    }
                    filter::COPY => pixels * tpixel.bytes,
                    other => return Err(bad(format!("Tight filter {other} is not written"))),
                };
                if want < 12 {
                    reader.take(want)?;
                } else {
                    let length = read_compact(&mut reader)?;
                    reader.take(length)?;
                }
            }
        }
        Ok(reader.at)
    }

    pub fn decode(
        &mut self,
        payload: &[u8],
        rect: Rect,
        pf: &PixelFormat,
        fb: &mut Framebuffer,
    ) -> Result<usize> {
        let tpixel = Tpixel::of(pf);
        let unpacker = Unpacker::new(pf);
        let mut reader = Reader::new(payload);
        let control = reader.byte()?;

        // The low four bits name streams the server has started again.
        for id in 0..4 {
            if control & (1 << id) != 0 {
                self.streams[id] = Decompress::new(true);
            }
        }

        let (width, height) = (rect.width(), rect.height());
        let pixels = (width * height) as usize;
        match control & 0xf0 {
            ctl::FILL => {
                let px = read_tpixel(&mut reader, tpixel, &unpacker)?;
                fb.fill(rect, px);
            }
            ctl::JPEG => {
                let length = read_compact(&mut reader)?;
                let data = reader.take(length)?;
                let (w, h, rgb) = decode_jpeg(data)?;
                if i32::from(w) != width || i32::from(h) != height {
                    return Err(bad(format!("a {w}x{h} JPEG in a {width}x{height} rectangle")));
                }
                for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
                    let (x, y) = (i as i32 % width, i as i32 / width);
                    fb.put_pixel(
                        (rect.x1 + x) as u32,
                        (rect.y1 + y) as u32,
                        [px[2], px[1], px[0], 0],
                    );
                }
            }
            _ => {
                let id = usize::from((control >> 4) & 0x03);
                let filter_id = if control & ctl::EXPLICIT_FILTER != 0 {
                    reader.byte()?
                } else {
                    filter::COPY
                };
                match filter_id {
                    filter::COPY => {
                        let want = pixels * tpixel.bytes;
                        let data = self.body(&mut reader, id, want)?;
                        let mut source = Reader::new(&data);
                        for i in 0..pixels {
                            let px = read_tpixel(&mut source, tpixel, &unpacker)?;
                            put(fb, rect, i, width, px);
                        }
                    }
                    filter::PALETTE => {
                        let size = usize::from(reader.byte()?) + 1;
                        let palette: Vec<[u8; 4]> = (0..size)
                            .map(|_| read_tpixel(&mut reader, tpixel, &unpacker))
                            .collect::<Result<_>>()?;
                        // Two colours are a bit a pixel, anything more is a
                        // byte, and either way the rows are padded.
                        let want = if size == 2 {
                            (width as usize).div_ceil(8) * height as usize
                        } else {
                            pixels
                        };
                        let data = self.body(&mut reader, id, want)?;
                        if size == 2 {
                            let per_row = (width as usize).div_ceil(8);
                            for y in 0..height {
                                let row = data
                                    .get(y as usize * per_row..)
                                    .ok_or_else(|| bad("mono rows ran out"))?;
                                for x in 0..width {
                                    let byte = row[(x / 8) as usize];
                                    let bit = (byte >> (7 - (x % 8))) & 1;
                                    fb.put_pixel(
                                        (rect.x1 + x) as u32,
                                        (rect.y1 + y) as u32,
                                        palette[usize::from(bit)],
                                    );
                                }
                            }
                        } else {
                            for (i, index) in data.iter().take(pixels).enumerate() {
                                let px = *palette
                                    .get(usize::from(*index))
                                    .ok_or_else(|| bad(format!("palette index {index} of {size}")))?;
                                put(fb, rect, i, width, px);
                            }
                        }
                    }
                    other => return Err(bad(format!("Tight filter {other} is not written"))),
                }
            }
        }
        Ok(reader.at)
    }

    /// The data after the filter: short enough to be sent as it is, or a
    /// compact length and a deflated block on this piece's stream.
    fn body(&mut self, reader: &mut Reader<'_>, id: usize, want: usize) -> Result<Vec<u8>> {
        if want < 12 {
            return Ok(reader.take(want)?.to_vec());
        }
        let length = read_compact(reader)?;
        let packed = reader.take(length)?;
        self.scratch.clear();
        let mut out = std::mem::take(&mut self.scratch);
        inflate(&mut self.streams[id], packed, &mut out)?;
        self.scratch = Vec::new();
        Ok(out)
    }
}

fn read_tpixel(reader: &mut Reader<'_>, tpixel: Tpixel, unpacker: &Unpacker) -> Result<[u8; 4]> {
    let bytes = reader.take(tpixel.bytes)?;
    if tpixel.three {
        // Red, green, blue in that order, whatever the client's own order.
        Ok([bytes[2], bytes[1], bytes[0], 0])
    } else {
        Ok(unpacker.pixel(bytes))
    }
}

/// A length in one to three bytes, seven bits each, the high bit saying
/// another follows.
fn read_compact(reader: &mut Reader<'_>) -> Result<usize> {
    let mut length = 0usize;
    for group in 0..3 {
        let byte = reader.byte()?;
        length |= usize::from(byte & 0x7f) << (group * 7);
        if byte & 0x80 == 0 {
            break;
        }
    }
    Ok(length)
}

fn decode_jpeg(data: &[u8]) -> Result<(u16, u16, Vec<u8>)> {
    let mut decoder = zune_jpeg::JpegDecoder::new(data);
    decoder
        .decode_headers()
        .map_err(|e| bad(format!("jpeg headers: {e:?}")))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| bad("a jpeg with no dimensions"))?;
    let (width, height) = (width as u16, height as u16);
    let pixels = decoder.decode().map_err(|e| bad(format!("jpeg: {e:?}")))?;
    Ok((width, height, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_row;

    #[test]
    fn a_format_round_trips_through_the_packer_and_back() {
        for pf in [
            PixelFormat::bgrx32(),
            PixelFormat {
                red_shift: 0,
                green_shift: 8,
                blue_shift: 16,
                ..PixelFormat::bgrx32()
            },
            PixelFormat {
                big_endian: true,
                ..PixelFormat::bgrx32()
            },
        ] {
            let unpacker = Unpacker::new(&pf);
            for px in [[0, 0, 0, 0], [255, 255, 255, 0], [1, 2, 3, 0], [10, 200, 30, 0]] {
                let mut packed = Vec::new();
                convert_row(&px, &pf, &mut packed);
                assert_eq!(unpacker.pixel(&packed), px, "{pf:?}");
            }
        }
    }

    #[test]
    fn a_narrow_format_lands_on_the_nearest_value_it_holds() {
        let pf = PixelFormat::rgb565();
        let unpacker = Unpacker::new(&pf);
        let mut packed = Vec::new();
        convert_row(&[0, 0, 255, 0], &pf, &mut packed);
        // Red survives whole; the others have nothing to lose.
        assert_eq!(unpacker.pixel(&packed), [0, 0, 255, 0]);
        packed.clear();
        convert_row(&[128, 128, 128, 0], &pf, &mut packed);
        let [b, g, r, _] = unpacker.pixel(&packed);
        for (channel, value) in [("blue", b), ("green", g), ("red", r)] {
            assert!(value.abs_diff(128) <= 5, "{channel} came back {value}");
        }
    }
}
