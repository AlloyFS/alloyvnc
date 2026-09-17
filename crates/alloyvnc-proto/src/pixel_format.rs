use crate::Error;

/// The 16-byte PIXEL_FORMAT of RFC 6143 section 7.4: how a client wants each
/// pixel laid out.
///
/// Only true-colour formats are accepted. A colour map was a memory saving
/// for 8-bit displays and nothing modern asks for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// Bytes on the wire, padding included.
    pub const WIRE_LEN: usize = 16;

    /// 32 bits per pixel, little-endian, blue in the lowest byte.
    ///
    /// This is what DXGI and X11 hand over and what TigerVNC and noVNC ask
    /// for on a 24-bit display, so the framebuffer is kept in it and the
    /// common case on the wire is a plain copy.
    pub const fn bgrx32() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_colour: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }

    /// 16 bits per pixel, 5-6-5, little-endian. What a viewer on a slow link
    /// falls back to.
    pub const fn rgb565() -> Self {
        Self {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        }
    }

    /// 8 bits per pixel, 2-3-3 with blue in the top bits: TigerVNC's lowest
    /// colour level.
    pub const fn bgr233() -> Self {
        Self {
            bits_per_pixel: 8,
            depth: 8,
            big_endian: false,
            true_colour: true,
            red_max: 7,
            green_max: 7,
            blue_max: 3,
            red_shift: 0,
            green_shift: 3,
            blue_shift: 6,
        }
    }

    pub const fn bytes_per_pixel(&self) -> usize {
        (self.bits_per_pixel / 8) as usize
    }

    pub fn parse(buf: &[u8]) -> Result<Self, Error> {
        if buf.len() < Self::WIRE_LEN {
            return Err(Error::Truncated("pixel format"));
        }
        let u16_at = |i: usize| u16::from_be_bytes([buf[i], buf[i + 1]]);
        Ok(Self {
            bits_per_pixel: buf[0],
            depth: buf[1],
            big_endian: buf[2] != 0,
            true_colour: buf[3] != 0,
            red_max: u16_at(4),
            green_max: u16_at(6),
            blue_max: u16_at(8),
            red_shift: buf[10],
            green_shift: buf[11],
            blue_shift: buf[12],
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.bits_per_pixel);
        out.push(self.depth);
        out.push(u8::from(self.big_endian));
        out.push(u8::from(self.true_colour));
        out.extend_from_slice(&self.red_max.to_be_bytes());
        out.extend_from_slice(&self.green_max.to_be_bytes());
        out.extend_from_slice(&self.blue_max.to_be_bytes());
        out.push(self.red_shift);
        out.push(self.green_shift);
        out.push(self.blue_shift);
        out.extend_from_slice(&[0, 0, 0]);
    }

    /// Refuse what the encoders cannot produce, before a session commits to it.
    pub fn validate(&self) -> Result<(), Error> {
        if !self.true_colour {
            return Err(Error::UnsupportedPixelFormat("colour map"));
        }
        if !matches!(self.bits_per_pixel, 8 | 16 | 32) {
            return Err(Error::UnsupportedPixelFormat(
                "bits per pixel must be 8, 16 or 32",
            ));
        }
        if self.depth > self.bits_per_pixel {
            return Err(Error::UnsupportedPixelFormat("depth exceeds bits per pixel"));
        }
        for (max, shift) in [
            (self.red_max, self.red_shift),
            (self.green_max, self.green_shift),
            (self.blue_max, self.blue_shift),
        ] {
            let range = u32::from(max) + 1;
            if max == 0 || !range.is_power_of_two() {
                return Err(Error::UnsupportedPixelFormat("channel max is not 2^n - 1"));
            }
            if u32::from(shift) + range.trailing_zeros() > u32::from(self.bits_per_pixel) {
                return Err(Error::UnsupportedPixelFormat("channel does not fit in the pixel"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgrx32_has_the_bytes_tigervnc_sends() {
        let mut out = Vec::new();
        PixelFormat::bgrx32().write(&mut out);
        assert_eq!(out, [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    }

    #[test]
    fn round_trips() {
        for pf in [
            PixelFormat::bgrx32(),
            PixelFormat::rgb565(),
            PixelFormat::bgr233(),
        ] {
            let mut out = Vec::new();
            pf.write(&mut out);
            assert_eq!(out.len(), PixelFormat::WIRE_LEN);
            assert_eq!(PixelFormat::parse(&out).unwrap(), pf);
            pf.validate().unwrap();
        }
    }

    #[test]
    fn refuses_what_cannot_be_encoded() {
        let colour_map = PixelFormat {
            true_colour: false,
            ..PixelFormat::bgrx32()
        };
        assert!(colour_map.validate().is_err());
        let odd_width = PixelFormat {
            bits_per_pixel: 24,
            ..PixelFormat::bgrx32()
        };
        assert!(odd_width.validate().is_err());
        let overflow = PixelFormat {
            red_shift: 25,
            ..PixelFormat::bgrx32()
        };
        assert!(overflow.validate().is_err());
        let ragged_max = PixelFormat {
            green_max: 200,
            ..PixelFormat::bgrx32()
        };
        assert!(ragged_max.validate().is_err());
        assert_eq!(
            PixelFormat::parse(&[0; 15]),
            Err(Error::Truncated("pixel format"))
        );
    }
}
