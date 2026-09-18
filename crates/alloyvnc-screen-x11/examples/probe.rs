//! What the X display says about itself, and whether a frame comes back.
//!
//! The one diagnostic worth having when a capture refuses to start or comes
//! back black. It names the extensions, the visual, the geometry and the
//! monitors, then takes a frame and reports how much of it is not black,
//! then waits for something to be drawn and reports the rectangles.
//!
//! `cargo run -p alloyvnc-screen-x11 --example probe [seconds] [input]`
//!
//! With `input` it also drives the pointer through XTEST and reads it
//! back, which is the only way to tell an injection that went nowhere
//! from one that worked. It moves the real pointer of the display it is
//! pointed at, so it is off by default.

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("X11 is Linux only here");
}

#[cfg(target_os = "linux")]
fn main() {
    use std::time::{Duration, Instant};

    use alloyvnc_screen::{Capture, Framebuffer};
    use alloyvnc_screen_x11::X11Capture;

    let seconds: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(5);
    if std::env::args().any(|a| a == "input") {
        check_input();
    }
    let mut capture = match X11Capture::new(None) {
        Ok(capture) => capture,
        Err(e) => {
            println!("cannot capture: {e}");
            return;
        }
    };
    let (width, height) = capture.size();
    println!("picture: {width}x{height}");
    for (i, screen) in capture.screens().iter().enumerate() {
        println!(
            "  monitor {i}: {},{} to {},{}",
            screen.x1, screen.y1, screen.x2, screen.y2
        );
    }

    let mut fb = Framebuffer::new(width, height);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut frames = 0u32;
    let mut rects = 0usize;
    while Instant::now() < deadline {
        match capture.wait(Duration::from_millis(200)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => {
                println!("wait: {e}");
                break;
            }
        }
        match capture.apply(&mut fb) {
            Ok(frame) => {
                frames += 1;
                rects += frame.damage.rects().len();
                if frames <= 5 {
                    println!(
                        "frame {frames}: {} rects, {} pixels, cursor {}, resized {}",
                        frame.damage.rects().len(),
                        frame.damage.rects().iter().map(|r| r.area()).sum::<i64>(),
                        frame.cursor.as_ref().map_or("none".into(), |c| format!(
                            "{}x{} hot {},{}",
                            c.width, c.height, c.hot_x, c.hot_y
                        )),
                        frame.resized
                    );
                }
            }
            Err(e) => {
                println!("apply: {e}");
                break;
            }
        }
    }

    // A rootless X server keeps no pixels of its own in the root window, so
    // a picture that is entirely one colour is the thing to look for before
    // blaming anything else.
    let data = fb.data();
    let black = data
        .chunks_exact(4)
        .filter(|px| px[0] == 0 && px[1] == 0 && px[2] == 0)
        .count();
    let total = data.len() / 4;
    println!(
        "after {seconds}s: {frames} frames, {rects} rectangles, {} of {total} pixels black ({}%)",
        black,
        black * 100 / total.max(1)
    );
    println!("centre pixel: {:?}", fb.pixel(width / 2, height / 2));
    println!("corner pixel: {:?}", fb.pixel(4, 4));
}

/// Move the pointer with XTEST and ask the server where it ended up.
#[cfg(target_os = "linux")]
fn check_input() {
    use alloyvnc_screen::Input;
    use alloyvnc_screen_x11::X11Input;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let mut input = match X11Input::new(None) {
        Ok(input) => input,
        Err(e) => {
            println!("cannot inject: {e}");
            return;
        }
    };
    let (conn, screen_num) = x11rb::connect(None).expect("a second connection to read back with");
    let root = conn.setup().roots[screen_num].root;
    for (x, y) in [(100u16, 80u16), (640, 360), (12, 700)] {
        input.pointer(x, y, 0);
        std::thread::sleep(std::time::Duration::from_millis(60));
        let at = conn.query_pointer(root).unwrap().reply().unwrap();
        let landed = (at.root_x as u16, at.root_y as u16) == (x, y);
        println!(
            "pointer to {x},{y}: server says {},{} {}",
            at.root_x,
            at.root_y,
            if landed { "ok" } else { "WRONG" }
        );
    }
}
