//! The numbers, as a document to curl.
//!
//! Counters that only reach a log line when a session ends are no use while
//! it is running, which is exactly when the question comes up: is the client
//! behind, is the window open, how long is a pixel taking to get from the
//! screen to the socket. So the server can hold a second listener whose only
//! answer is one JSON object of everything it knows right now.
//!
//! Written by hand, both the HTTP and the JSON. Serde and a web framework
//! would be two large dependencies and a build minute for a document of
//! about forty numbers with no user input in it beyond "did you say GET",
//! and this way the whole endpoint is one file with nothing behind it.
//!
//! It is off unless asked for, and loopback unless the address says
//! otherwise, the same rule the RFB listener follows: these numbers say how
//! busy a desk is and what its screen costs to send, which is nobody else's
//! business.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::shared::Shared;

/// Milliseconds a frame took to get from the capture thread into the
/// socket, counted in buckets rather than averaged: a mean hides the tail,
/// and the tail is what somebody watching the screen actually notices.
#[derive(Debug, Default)]
pub struct Latency {
    pub le1: AtomicU64,
    pub le2: AtomicU64,
    pub le5: AtomicU64,
    pub le10: AtomicU64,
    pub le20: AtomicU64,
    pub le50: AtomicU64,
    pub le100: AtomicU64,
    pub over: AtomicU64,
}

impl Latency {
    pub fn record(&self, age: Duration) {
        let ms = age.as_millis();
        let bucket = match ms {
            0..=1 => &self.le1,
            2 => &self.le2,
            3..=5 => &self.le5,
            6..=10 => &self.le10,
            11..=20 => &self.le20,
            21..=50 => &self.le50,
            51..=100 => &self.le100,
            _ => &self.over,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
    }

    fn write(&self, out: &mut String) {
        let _ = write!(
            out,
            "{{\"le1\":{},\"le2\":{},\"le5\":{},\"le10\":{},\"le20\":{},\"le50\":{},\"le100\":{},\"over\":{}}}",
            self.le1.load(Ordering::Relaxed),
            self.le2.load(Ordering::Relaxed),
            self.le5.load(Ordering::Relaxed),
            self.le10.load(Ordering::Relaxed),
            self.le20.load(Ordering::Relaxed),
            self.le50.load(Ordering::Relaxed),
            self.le100.load(Ordering::Relaxed),
            self.over.load(Ordering::Relaxed),
        );
    }
}

/// One live session's counters. The session owns one of these and the
/// registry holds a second reference, so reading them never touches the
/// session's own state or waits on anything it is doing.
#[derive(Debug)]
pub struct SessionStats {
    /// The client's address, which is the only string in the document and
    /// needs no escaping: an address is digits, dots, colons and brackets.
    pub peer: String,
    pub updates: AtomicU64,
    pub rects: AtomicU64,
    pub copy_rects: AtomicU64,
    pub cursor_rects: AtomicU64,
    pub bytes: AtomicU64,
    pub in_flight: AtomicU64,
    /// Zero means no round trip has come back yet.
    pub rtt_micros: AtomicU64,
    pub base_rtt_micros: AtomicU64,
    pub window: AtomicU64,
    pub pings_outstanding: AtomicU64,
    pub latency: Latency,
}

impl SessionStats {
    pub fn new(peer: SocketAddr) -> Arc<SessionStats> {
        Arc::new(SessionStats {
            peer: peer.to_string(),
            updates: AtomicU64::new(0),
            rects: AtomicU64::new(0),
            copy_rects: AtomicU64::new(0),
            cursor_rects: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            rtt_micros: AtomicU64::new(0),
            base_rtt_micros: AtomicU64::new(0),
            window: AtomicU64::new(0),
            pings_outstanding: AtomicU64::new(0),
            latency: Latency::default(),
        })
    }

    fn write(&self, out: &mut String) {
        let ms = |micros: u64| micros as f64 / 1000.0;
        let _ = write!(
            out,
            "{{\"peer\":\"{}\",\"updates\":{},\"rects\":{},\"copy_rects\":{},\"cursor_rects\":{},\
             \"bytes\":{},\"in_flight\":{},\"rtt_ms\":{:.3},\"base_rtt_ms\":{:.3},\"window\":{},\
             \"pings_outstanding\":{},\"latency_ms\":",
            self.peer,
            self.updates.load(Ordering::Relaxed),
            self.rects.load(Ordering::Relaxed),
            self.copy_rects.load(Ordering::Relaxed),
            self.cursor_rects.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.in_flight.load(Ordering::Relaxed),
            ms(self.rtt_micros.load(Ordering::Relaxed)),
            ms(self.base_rtt_micros.load(Ordering::Relaxed)),
            self.window.load(Ordering::Relaxed),
            self.pings_outstanding.load(Ordering::Relaxed),
        );
        self.latency.write(out);
        out.push('}');
    }
}

/// What the capture thread has done since the server started.
#[derive(Debug, Default)]
pub struct CaptureStats {
    pub frames: AtomicU64,
    /// Frames the compare pass found nothing in, which is the count that
    /// says how much a backend over-reports.
    pub empty: AtomicU64,
    pub reported: AtomicU64,
    pub tightened: AtomicU64,
    pub moves: AtomicU64,
    pub micros: AtomicU64,
}

/// The whole document.
pub fn document(shared: &Shared) -> String {
    let (width, height) = {
        let fb = shared.fb.read();
        (fb.width(), fb.height())
    };
    let c = &shared.capture;
    let mut out = String::with_capacity(1024);
    let _ = write!(
        out,
        "{{\"capture\":{{\"frames\":{},\"empty\":{},\"reported\":{},\"tightened\":{},\"moves\":{},\
         \"micros\":{},\"seq\":{},\"width\":{},\"height\":{}}},\"sessions\":[",
        c.frames.load(Ordering::Relaxed),
        c.empty.load(Ordering::Relaxed),
        c.reported.load(Ordering::Relaxed),
        c.tightened.load(Ordering::Relaxed),
        c.moves.load(Ordering::Relaxed),
        c.micros.load(Ordering::Relaxed),
        shared.seq(),
        width,
        height,
    );
    // The lock is held only to copy the handles out, never across the
    // formatting: a session must not wait on somebody reading a web page.
    let sessions: Vec<Arc<SessionStats>> = shared.sessions.lock().clone();
    for (i, session) in sessions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        session.write(&mut out);
    }
    out.push_str("]}");
    out
}

/// The endpoint's listener, the same shape as the RFB one so a test can
/// bind it on port zero and ask what it got.
pub struct Endpoint {
    listener: TcpListener,
    shared: Arc<Shared>,
}

impl Endpoint {
    pub async fn bind(addr: SocketAddr, shared: Arc<Shared>) -> Result<Endpoint> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind the stats endpoint to {addr}"))?;
        Ok(Endpoint { listener, shared })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    pub async fn run(self) -> Result<()> {
        loop {
            let (stream, peer) = self
                .listener
                .accept()
                .await
                .context("accept on the stats endpoint")?;
            let shared = self.shared.clone();
            tokio::spawn(async move {
                if let Err(e) = answer(stream, &shared).await {
                    tracing::debug!(%peer, error = %e, "stats request failed");
                }
            });
        }
    }
}

/// The longest request line and headers this will read before giving up. A
/// stats endpoint has no use for a large request and every use for a small
/// ceiling.
const MAX_REQUEST: usize = 8 * 1024;

async fn answer(mut stream: TcpStream, shared: &Shared) -> Result<()> {
    let mut buf = Vec::with_capacity(512);
    // Enough of the request to see the method, and no more: the headers are
    // read to the blank line so the client is not writing into a closed
    // socket, and the body is never wanted.
    loop {
        let mut chunk = [0u8; 512];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..buf.len().min(256)]);
    let response = if head.starts_with("GET ") {
        let body = document(shared);
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    } else {
        let body = "{\"error\":\"only GET\"}";
        format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    };
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buckets_take_the_edges() {
        let l = Latency::default();
        for ms in [0, 1, 2, 3, 5, 6, 10, 11, 20, 21, 50, 51, 100, 101, 5000] {
            l.record(Duration::from_millis(ms));
        }
        let mut out = String::new();
        l.write(&mut out);
        assert_eq!(
            out,
            "{\"le1\":2,\"le2\":1,\"le5\":2,\"le10\":2,\"le20\":2,\"le50\":2,\"le100\":2,\"over\":2}"
        );
    }

    #[test]
    fn a_session_reads_as_one_object() {
        let stats = SessionStats::new("127.0.0.1:5900".parse().unwrap());
        stats.updates.store(7, Ordering::Relaxed);
        stats.rtt_micros.store(12_500, Ordering::Relaxed);
        stats.window.store(65536, Ordering::Relaxed);
        let mut out = String::new();
        stats.write(&mut out);
        assert!(
            out.starts_with("{\"peer\":\"127.0.0.1:5900\",\"updates\":7,"),
            "{out}"
        );
        assert!(out.contains("\"rtt_ms\":12.500"), "{out}");
        assert!(out.contains("\"window\":65536"), "{out}");
        assert!(out.ends_with("\"over\":0}}"), "{out}");
        // Balanced braces, which is as much of a parser as this needs.
        assert_eq!(out.matches('{').count(), out.matches('}').count());
    }
}
