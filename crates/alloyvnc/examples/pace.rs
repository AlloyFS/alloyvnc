//! What the flow control does when a client cannot keep up.
//!
//! Two runs of the same thirty seconds against the synthetic screen, in
//! process, with everything the server and the client would do over a real
//! socket except the network. One client reads as fast as it can; the other
//! reads at about a tenth of that pace, which is what a viewer on a slow
//! link or a busy machine looks like from here.
//!
//! What to look for: the slow client should receive far fewer updates than
//! the frames it missed, because the ones it did not get folded into the
//! ones it did, and its latency histogram should stay bounded rather than
//! growing without limit, because the window stopped the server queueing
//! updates it could not take.
//!
//! `cargo run --release -p alloyvnc --example pace`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alloyvnc::client::Client;
use alloyvnc::server::Server;
use alloyvnc::session::SessionConfig;
use alloyvnc::shared::Shared;
use alloyvnc::stats;
use alloyvnc_proto::encoding;
use alloyvnc_screen::synth::{Pace, Synth};
use alloyvnc_screen::{NullClipboard, NullInput};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 30;
const SECONDS: u64 = 30;

const ALL: &[i32] = &[
    encoding::RAW,
    encoding::COPY_RECT,
    encoding::PSEUDO_CURSOR_WITH_ALPHA,
    encoding::PSEUDO_EXTENDED_DESKTOP_SIZE,
    encoding::PSEUDO_CONTINUOUS_UPDATES,
    encoding::PSEUDO_FENCE,
];

#[tokio::main]
async fn main() {
    // Adjacently, and in this order both times, so nothing about the
    // machine's mood between them is read as a difference in the code.
    for (label, pause) in [("fast", Duration::ZERO), ("slow", Duration::from_millis(300))] {
        run(label, pause).await;
    }
}

async fn run(label: &str, pause: Duration) {
    let shared = Shared::new(
        "pace",
        WIDTH,
        HEIGHT,
        Box::new(NullInput),
        Box::new(NullClipboard),
    );
    let capture = Synth::new(WIDTH, HEIGHT, Pace::Fps(FPS));
    let server = Server::bind(
        "127.0.0.1:0".parse().expect("a loopback address"),
        SessionConfig::default(),
        shared.clone(),
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("the bound address");
    tokio::spawn(server.run());
    let stop = Arc::new(AtomicBool::new(false));
    let capture_thread = alloyvnc::capture::spawn(shared.clone(), Box::new(capture), stop.clone());

    let mut client = Client::connect(addr, None).await.expect("connect");
    client.set_encodings(ALL).await.expect("encodings");
    // Pushed rather than pulled, so the pace is the client's reading and
    // not its asking: a client that asks slowly is not behind, it is
    // polite, and the window has nothing to say about it.
    client
        .enable_continuous_updates(true)
        .await
        .expect("continuous updates");

    let started = Instant::now();
    let deadline = started + Duration::from_secs(SECONDS);
    let mut updates = 0u64;
    while Instant::now() < deadline {
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        match tokio::time::timeout(Duration::from_millis(500), client.next_update()).await {
            Ok(Ok(_)) => updates += 1,
            Ok(Err(e)) => {
                eprintln!("{label}: the session ended: {e}");
                break;
            }
            Err(_) => {}
        }
    }
    let elapsed = started.elapsed();

    let document = stats::document(&shared);
    println!("--- {label}: {SECONDS} s, screen {WIDTH}x{HEIGHT} at {FPS} fps ---");
    println!(
        "client read {updates} updates in {:.1} s ({:.1} a second), answered {} pings",
        elapsed.as_secs_f64(),
        updates as f64 / elapsed.as_secs_f64(),
        client.pings
    );
    println!("{document}");

    stop.store(true, Ordering::Relaxed);
    let _ = capture_thread.join();
}
