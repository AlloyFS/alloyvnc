//! What each encoding costs and saves, on the same three hundred frames.
//!
//! The synthetic screen is deterministic, so these numbers compare across
//! days and across changes. Every encoder sees exactly the same rectangles
//! of exactly the same pictures, in the same order, and keeps its own
//! streams across the run the way a session does.
//!
//! Two passes. The first uses the screen's own damage, which is precise; the
//! second uses its coarse mode, one rectangle around everything that
//! changed, which is what DXGI and X11 actually report. The difference
//! between the two tables is what the compare pass of phase 2a is worth to
//! each encoding.
//!
//! `cargo run --release -p alloyvnc --example encoders`

use std::time::{Duration, Instant};

use alloyvnc_encode::hextile::Hextile;
use alloyvnc_encode::tight::Tight;
use alloyvnc_encode::zrle::Zrle;
use alloyvnc_encode::{Framebuffer, PixelFormat, raw};
use alloyvnc_region::Rect;
use alloyvnc_screen::Capture;
use alloyvnc_screen::synth::{Pace, Step, Synth};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FRAMES: u64 = 300;

enum Coder {
    Raw,
    Hextile(Box<Hextile>),
    Zrle(Box<Zrle>),
    Tight(Box<Tight>),
}

impl Coder {
    fn encode(&mut self, fb: &Framebuffer, rect: Rect, pf: &PixelFormat, out: &mut Vec<u8>) {
        match self {
            Coder::Raw => raw::encode(fb, rect, pf, out),
            Coder::Hextile(h) => h.encode(fb, rect, pf, out),
            Coder::Zrle(z) => z.encode(fb, rect, pf, out),
            Coder::Tight(t) => {
                for piece in Tight::piece_rects(rect) {
                    t.encode(fb, piece, pf, out);
                }
            }
        }
    }
}

struct Run {
    encoder: &'static str,
    format: &'static str,
    coder: Coder,
    pf: PixelFormat,
    bytes: u64,
    time: Duration,
}

fn formats() -> Vec<(&'static str, PixelFormat)> {
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
    ]
}

fn runs(quality: Option<u8>) -> Vec<Run> {
    let mut out = Vec::new();
    for (format, pf) in formats() {
        for encoder in ["Raw", "Hextile", "ZRLE", "Tight"] {
            let coder = match encoder {
                "Hextile" => Coder::Hextile(Box::new(Hextile::new())),
                "ZRLE" => Coder::Zrle(Box::new(Zrle::new(1))),
                "Tight" => Coder::Tight(Box::new(Tight::new(1, quality))),
                _ => Coder::Raw,
            };
            out.push(Run {
                encoder,
                format,
                coder,
                pf,
                bytes: 0,
                time: Duration::ZERO,
            });
        }
    }
    out
}

fn main() {
    println!("{FRAMES} frames of the synthetic screen at {WIDTH}x{HEIGHT}\n");
    table("precise damage, as the screen reports it", false, None);
    table("coarse damage, as DXGI and X11 report it", true, None);
    // The third table comes out byte for byte the same as the first, which
    // is the finding rather than a fault: every piece of this screen fits a
    // palette, and a palette is both smaller and lossless, so the quality
    // level never gets a look in. JPEG is proved on a gradient instead, in
    // the encode crate's tests and in the end-to-end one.
    table(
        "precise damage, Tight offered JPEG at quality 6 (never taken: a palette always wins here)",
        false,
        Some(6),
    );
}

fn table(title: &str, coarse: bool, quality: Option<u8>) {
    let step = Step::new();
    let mut synth = Synth::new(WIDTH, HEIGHT, Pace::Manual(step.clone()));
    if coarse {
        synth = synth.coarse();
    }
    let mut fb = Framebuffer::new(WIDTH, HEIGHT);
    let mut runs = runs(quality);
    let mut out = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    let mut pixels = 0u64;

    step.advance(FRAMES);
    for _ in 0..FRAMES {
        assert!(synth.wait(Duration::from_millis(50)).expect("the screen draws"));
        let frame = synth.apply(&mut fb).expect("the screen draws");
        pixels += frame.damage.area() as u64;
        for run in &mut runs {
            let started = Instant::now();
            out.clear();
            for rect in frame.damage.rects() {
                run.coder.encode(&fb, *rect, &run.pf, &mut out);
            }
            run.time += started.elapsed();
            run.bytes += out.len() as u64;
        }
    }

    println!("{title}");
    println!("  {pixels} pixels of damage over {FRAMES} frames");
    println!(
        "  {:<9} {:<8} {:>12} {:>9} {:>10} {:>10}",
        "encoder", "format", "bytes", "ms", "ms/frame", "of Raw"
    );
    for run in &runs {
        let raw = runs
            .iter()
            .find(|r| r.encoder == "Raw" && r.format == run.format)
            .map(|r| r.bytes)
            .unwrap_or(1)
            .max(1);
        let ms = run.time.as_secs_f64() * 1000.0;
        println!(
            "  {:<9} {:<8} {:>12} {:>9.1} {:>10.3} {:>9.1}%",
            run.encoder,
            run.format,
            run.bytes,
            ms,
            ms / FRAMES as f64,
            run.bytes as f64 * 100.0 / raw as f64,
        );
    }
    println!();
}
