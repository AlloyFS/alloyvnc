//! JPEG for the parts of a screen that are photographs.
//!
//! Every other encoding here is lossless and works by finding repetition.
//! A photograph, a video frame or a gradient has none, so they all fall back
//! to sending the pixels. JPEG is the one that does not, and on the areas it
//! suits it is an order of magnitude smaller than anything else.
//!
//! **It takes RGB from the framebuffer, never the client's format.** JPEG
//! has its own colour space and its own precision; handing it a pixel that
//! has already been squeezed into five bits of red would lose that detail
//! twice, once on the way in and again in the transform. The client's format
//! is what the *lossless* paths pack into, and it is irrelevant here: a JPEG
//! rectangle carries its own colour and the client's decoder produces what
//! it produces.
//!
//! The encoder is [jpeg-encoder], pure Rust. libjpeg-turbo through the
//! turbojpeg crate is four to six times faster and was tried first; it
//! builds libjpeg-turbo from source and needs cmake and nasm, neither of
//! which is on this machine.
//!
//! [jpeg-encoder]: https://docs.rs/jpeg-encoder

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

/// Quality levels 0 to 9 as TigerVNC sets them, which is what a client
/// asking for TightQuality expects to get.
const QUALITY: [u8; 10] = [15, 29, 41, 42, 62, 77, 79, 86, 92, 100];

/// How much colour is thrown away before the transform. Chroma is the part
/// of a picture an eye is worst at, so the low levels halve it in both
/// directions and only the top three keep it whole.
fn sampling(level: u8) -> SamplingFactor {
    match level {
        0..=3 => SamplingFactor::F_2_2,
        4..=6 => SamplingFactor::F_2_1,
        _ => SamplingFactor::F_1_1,
    }
}

pub fn quality(level: u8) -> u8 {
    QUALITY[usize::from(level.min(9))]
}

/// Encode `rgb`, three bytes a pixel, at one of the ten levels.
pub fn encode(rgb: &[u8], width: u16, height: u16, level: u8) -> Result<Vec<u8>, String> {
    let level = level.min(9);
    let mut out = Vec::with_capacity(rgb.len() / 8);
    let mut encoder = Encoder::new(&mut out, quality(level));
    encoder.set_sampling_factor(sampling(level));
    encoder
        .encode(rgb, width, height, ColorType::Rgb)
        .map_err(|e| format!("jpeg: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_run_from_coarse_to_whole() {
        assert_eq!(quality(0), 15);
        assert_eq!(quality(6), 79);
        assert_eq!(quality(9), 100);
        assert_eq!(quality(200), 100, "anything past nine is nine");
        assert!(matches!(sampling(0), SamplingFactor::F_2_2));
        assert!(matches!(sampling(5), SamplingFactor::F_2_1));
        assert!(matches!(sampling(9), SamplingFactor::F_1_1));
    }

    #[test]
    fn a_higher_level_costs_more_bytes() {
        // A gradient, which is what JPEG is for and what a palette cannot
        // help with.
        let (w, h) = (64u16, 64u16);
        let mut rgb = Vec::with_capacity(usize::from(w) * usize::from(h) * 3);
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8]);
            }
        }
        let low = encode(&rgb, w, h, 1).expect("encodes");
        let high = encode(&rgb, w, h, 9).expect("encodes");
        assert!(low.len() < high.len(), "{} against {}", low.len(), high.len());
        assert_eq!(&low[..2], &[0xff, 0xd8], "a JPEG starts with its marker");
    }
}
