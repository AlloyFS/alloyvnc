use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloyvnc::server::Server;
use alloyvnc::session::SessionConfig;
use alloyvnc::shared::Shared;
use alloyvnc_screen::synth::{Pace, Synth};
use alloyvnc_screen::{Capture, Input, NullInput};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "alloyvnc",
    version,
    about = "A VNC server that sends only what changed."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Export a screen over RFB.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Address to listen on. Anything but loopback needs a password or --insecure.
    #[arg(long, default_value = "127.0.0.1:5900")]
    bind: SocketAddr,

    /// What to capture.
    #[arg(long, value_enum, default_value_t = Backend::Synth)]
    backend: Backend,

    /// Size of the synthetic screen, as WIDTHxHEIGHT.
    #[arg(long, default_value = "1280x720")]
    size: String,

    /// Frame rate of the synthetic screen.
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// VNC password; the first eight characters count. Also read from ALLOYVNC_PASSWORD.
    #[arg(long, env = "ALLOYVNC_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// Desktop name shown by the viewer.
    #[arg(long, default_value = "alloyvnc")]
    name: String,

    /// Allow a non-loopback bind with no password.
    #[arg(long)]
    insecure: bool,

    /// Ceiling on updates per second to one client.
    #[arg(long, default_value_t = 60)]
    max_fps: u32,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    /// A deterministic animated test pattern.
    Synth,
    /// The Windows desktop, through DXGI duplication.
    #[cfg(windows)]
    Dxgi,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything reads a coordinate: the process has to be shown
    // physical pixels or DXGI and the pointer would disagree about where
    // the screen is.
    #[cfg(windows)]
    alloyvnc_screen_dxgi::init();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    if !args.bind.ip().is_loopback() && args.password.is_none() && !args.insecure {
        bail!(
            "{} is reachable from the network: set --password, or --insecure to mean it",
            args.bind
        );
    }
    let (width, height) = parse_size(&args.size)?;
    let (capture, input): (Box<dyn Capture>, Box<dyn Input>) = match args.backend {
        Backend::Synth => (
            Box::new(Synth::new(width, height, Pace::Fps(args.fps))),
            Box::new(NullInput),
        ),
        // The picture is whatever the monitors make; --size is the
        // synthetic screen's alone.
        #[cfg(windows)]
        Backend::Dxgi => {
            let capture = open_dxgi()?;
            let input = alloyvnc_screen_dxgi::WinInput::new(capture.origin());
            (Box::new(capture), Box::new(input))
        }
    };
    let (width, height) = capture.size();
    let shared = Shared::new(args.name, width, height, input);
    let session = SessionConfig {
        password: args.password,
        max_fps: args.max_fps,
        auth_fail_delay: Duration::from_secs(1),
    };
    let server = Server::bind(args.bind, session, shared.clone()).await?;
    tracing::info!(addr = %server.local_addr()?, width, height, "listening");

    let stop = Arc::new(AtomicBool::new(false));
    let capture_thread = alloyvnc::capture::spawn(shared, capture, stop.clone());
    let outcome = tokio::select! {
        r = server.run() => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("stopping");
            Ok(())
        }
    };
    stop.store(true, Ordering::Relaxed);
    let _ = capture_thread.join();
    outcome
}

/// Duplicating the desktop can be refused for a moment: a UAC prompt is up,
/// the session is switching, or another process holds the desktop image (a
/// RealVNC service on this machine did, now and then, for a few seconds).
/// A lost duplication means "try again", so the start does, briefly, before
/// giving up.
#[cfg(windows)]
fn open_dxgi() -> Result<alloyvnc_screen_dxgi::DxgiCapture> {
    use alloyvnc_screen::CaptureError;
    const TRIES: u32 = 5;
    let mut last = None;
    for attempt in 1..=TRIES {
        match alloyvnc_screen_dxgi::DxgiCapture::new() {
            Ok(capture) => return Ok(capture),
            Err(e @ CaptureError::Lost(_)) => {
                tracing::warn!(attempt, tries = TRIES, error = %e, "desktop duplication refused; retrying");
                last = Some(e);
                std::thread::sleep(Duration::from_millis(400));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last.expect("at least one attempt was made")).context("desktop duplication kept being refused")
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once('x')
        .with_context(|| format!("size {s:?} is not WIDTHxHEIGHT"))?;
    let w: u32 = w.parse().with_context(|| format!("width {w:?}"))?;
    let h: u32 = h.parse().with_context(|| format!("height {h:?}"))?;
    if !(128..=16384).contains(&w) || !(128..=16384).contains(&h) {
        bail!("size {s} is outside 128 to 16384 on a side");
    }
    Ok((w, h))
}
