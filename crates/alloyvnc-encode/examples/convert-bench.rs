//! What pixel-format conversion costs, per 1080p frame.
//!
//! The question the backlog held the swizzle behind: a client asking for the
//! framebuffer's own layout is a copy and costs nothing worth measuring, but
//! noVNC asks for RGBX, which is the same bytes in another order, and until
//! now that went through the general packer a pixel at a time. This says
//! what that was costing and what the shuffle saves.
//!
//! The rounds alternate so neither reading can be the machine warming up or
//! cooling down, and the copy is the control: if it moves between rounds,
//! nothing else in the run means anything either.
//!
//! `cargo run --release -p alloyvnc-encode --example convert-bench`

use std::time::Instant;

use alloyvnc_encode::Framebuffer;
use alloyvnc_encode::convert::{Packer, convert_row};
use alloyvnc_proto::PixelFormat;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const ROUNDS: usize = 15;

fn main() {
    let mut fb = Framebuffer::new(WIDTH, HEIGHT);
    // Something with every byte value in it, so nothing folds away.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let v = state.to_le_bytes();
            fb.put_pixel(x, y, [v[0], v[1], v[2], 0xff]);
        }
    }

    let native = PixelFormat::bgrx32();
    let rgbx = PixelFormat {
        red_shift: 0,
        green_shift: 8,
        blue_shift: 16,
        ..PixelFormat::bgrx32()
    };
    let packer = Packer::new(&rgbx);
    let mut out = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);

    // The fastest of several passes per variant, not the mean: noise on a
    // laptop only ever adds time, so the smallest reading is the one with
    // least of somebody else's work in it. The copy is the control; if it
    // moves between runs, nothing else in the table means anything.
    let mut best = [f64::MAX; 5];
    for _ in 0..ROUNDS {
        let mut take = |slot: usize, ms: f64| best[slot] = best[slot].min(ms);
        take(
            0,
            time(&mut out, |out| {
                for y in 0..HEIGHT {
                    convert_row(fb.row(y), &native, out);
                }
            }),
        );
        take(
            1,
            time(&mut out, |out| {
                for y in 0..HEIGHT {
                    shuffle(fb.row(y), out);
                }
            }),
        );
        take(
            2,
            time(&mut out, |out| {
                for y in 0..HEIGHT {
                    let row = fb.row(y);
                    out.reserve(row.len());
                    for px in row.as_chunks::<4>().0 {
                        out.extend_from_slice(&packer.pack(px).to_le_bytes());
                    }
                }
            }),
        );
        take(
            3,
            time(&mut out, |out| {
                for y in 0..HEIGHT {
                    let row = fb.row(y);
                    out.reserve(row.len());
                    for px in row.as_chunks::<4>().0 {
                        let v = (px[2] as u32) | (px[1] as u32) << 8 | (px[0] as u32) << 16;
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }),
        );
        take(
            4,
            time(&mut out, |out| {
                for y in 0..HEIGHT {
                    convert_row(fb.row(y), &PixelFormat::rgb565(), out);
                }
            }),
        );
    }

    println!("one 1920x1080 frame, best of {ROUNDS}");
    println!("{:<28} {:>8} {:>10}", "path", "ms", "MB/s");
    for (label, ms) in [
        ("native, a copy", best[0]),
        ("rgbx, shuffled", best[1]),
        ("rgbx, packed", best[2]),
        ("rgbx, constant shifts", best[3]),
        ("rgb565, packed", best[4]),
    ] {
        report(label, ms);
    }
}

/// The byte shuffle that did not earn its place: four moves a pixel, the
/// positions constant. Kept here because a measurement nobody can rerun is
/// an opinion.
fn shuffle(src: &[u8], out: &mut Vec<u8>) {
    out.reserve(src.len());
    for px in src.as_chunks::<4>().0 {
        let from = [px[0], px[1], px[2], px[3], 0];
        out.extend_from_slice(&[from[2], from[1], from[0], from[4]]);
    }
}

fn time(out: &mut Vec<u8>, mut work: impl FnMut(&mut Vec<u8>)) -> f64 {
    out.clear();
    let started = Instant::now();
    work(out);
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    // Touched so the work cannot be optimised away entirely.
    std::hint::black_box(out.len());
    elapsed
}

fn report(label: &str, ms: f64) {
    let bytes = WIDTH as f64 * HEIGHT as f64 * 4.0;
    println!("{label:<28} {ms:>8.2} {:>10.0}", bytes / ms / 1000.0);
}
