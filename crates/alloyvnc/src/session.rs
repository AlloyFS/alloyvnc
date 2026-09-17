//! One client, from handshake to hangup.
//!
//! The session keeps a pending damage region of its own, fed by the capture
//! thread's events, and sends one FramebufferUpdate whenever the client has
//! asked for one (or is in continuous mode) and something in the asked-for
//! area is pending. Everything pending goes in one update, so a slow client
//! sees fewer and larger updates rather than a queue of stale ones.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloyvnc_encode::raw;
use alloyvnc_proto::handshake::{self, Flow, ServerInit, security};
use alloyvnc_proto::msg::{self, ClientMessage};
use alloyvnc_proto::{PixelFormat, auth, encoding};
use alloyvnc_region::{Rect, Region};
use anyhow::{Context, Result, bail};
use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast;

use crate::shared::{DamageEvent, Shared};

/// What every session is told at accept time.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// A password turns on VNC authentication; none offers the None type.
    pub password: Option<String>,
    /// Updates to one client are never sent closer together than this.
    pub max_fps: u32,
    /// The pause before a failed authentication is answered, so a guess
    /// costs time.
    pub auth_fail_delay: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            password: None,
            max_fps: 60,
            auth_fail_delay: Duration::from_secs(1),
        }
    }
}

pub async fn run(
    mut stream: TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
    cfg: Arc<SessionConfig>,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let flow = negotiate_version(&mut stream).await?;
    authenticate(&mut stream, flow, &cfg).await?;
    let _shared_flag = stream.read_u8().await.context("client init")?;

    let (width, height) = {
        let fb = shared.fb.read();
        (fb.width(), fb.height())
    };
    let mut out = Vec::with_capacity(64);
    ServerInit {
        width: width as u16,
        height: height as u16,
        pixel_format: PixelFormat::bgrx32(),
        name: shared.name.clone(),
    }
    .write(&mut out);
    stream.write_all(&out).await?;
    tracing::info!(%peer, ?flow, width, height, "session up");

    let damage_rx = shared.damage.subscribe();
    let (rd, wr) = stream.into_split();
    let bounds = Rect::new(0, 0, width as i32, height as i32);
    let session = Session {
        shared,
        cfg,
        peer,
        pf: PixelFormat::bgrx32(),
        encodings: Vec::new(),
        pending: Region::from_rect(bounds),
        requested: None,
        continuous: None,
        continuous_offered: false,
        last_update: None,
        damage_rx,
        out,
    };
    session.serve(rd, wr).await
}

async fn negotiate_version(stream: &mut TcpStream) -> Result<Flow> {
    stream.write_all(&Flow::V38.version_bytes()).await?;
    let mut buf = [0u8; handshake::VERSION_LEN];
    stream.read_exact(&mut buf).await.context("protocol version")?;
    let (major, minor) = handshake::parse_version(&buf)?;
    Ok(Flow::for_version(major, minor)?)
}

async fn authenticate(stream: &mut TcpStream, flow: Flow, cfg: &SessionConfig) -> Result<()> {
    let offered: &[u8] = if cfg.password.is_some() {
        &[security::VNC_AUTH]
    } else {
        &[security::NONE]
    };
    let mut out = Vec::with_capacity(32);
    let chosen = match flow {
        Flow::V33 => {
            handshake::write_security_type_v33(&mut out, offered[0]);
            stream.write_all(&out).await?;
            offered[0]
        }
        Flow::V37 | Flow::V38 => {
            handshake::write_security_types(&mut out, offered);
            stream.write_all(&out).await?;
            let chosen = stream.read_u8().await.context("security type")?;
            if !offered.contains(&chosen) {
                out.clear();
                handshake::write_security_result(&mut out, flow, false, "security type not offered");
                stream.write_all(&out).await?;
                bail!("client chose security type {chosen}, offered {offered:?}");
            }
            chosen
        }
    };
    out.clear();
    match chosen {
        security::NONE => {
            // Only 3.8 confirms the None type; earlier flows go straight on.
            if flow == Flow::V38 {
                handshake::write_security_result(&mut out, flow, true, "");
                stream.write_all(&out).await?;
            }
        }
        security::VNC_AUTH => {
            let password = cfg.password.as_deref().unwrap_or_default();
            let challenge = auth::challenge();
            stream.write_all(&challenge).await?;
            let mut response = [0u8; auth::RESPONSE_LEN];
            stream.read_exact(&mut response).await.context("auth response")?;
            if !auth::verify(password, &challenge, &response) {
                tokio::time::sleep(cfg.auth_fail_delay).await;
                handshake::write_security_result(&mut out, flow, false, "authentication failed");
                stream.write_all(&out).await?;
                bail!("authentication failed");
            }
            handshake::write_security_result(&mut out, flow, true, "");
            stream.write_all(&out).await?;
        }
        other => bail!("security type {other} offered but not implemented"),
    }
    Ok(())
}

struct Session {
    shared: Arc<Shared>,
    cfg: Arc<SessionConfig>,
    peer: SocketAddr,
    pf: PixelFormat,
    encodings: Vec<i32>,
    /// What has changed since this client last saw it.
    pending: Region,
    /// The area the client's outstanding update requests cover.
    requested: Option<Rect>,
    /// The area continuous updates were enabled for.
    continuous: Option<Rect>,
    continuous_offered: bool,
    last_update: Option<Instant>,
    damage_rx: broadcast::Receiver<DamageEvent>,
    out: Vec<u8>,
}

impl Session {
    fn bounds(&self) -> Rect {
        self.shared.fb.read().bounds()
    }

    /// Where an update may go right now, if anywhere.
    fn due_area(&self) -> Option<Rect> {
        match (self.requested, self.continuous) {
            (Some(r), Some(c)) => Some(r.union_bounds(&c)),
            (Some(r), None) => Some(r),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }

    async fn serve(mut self, mut rd: OwnedReadHalf, mut wr: OwnedWriteHalf) -> Result<()> {
        let min_interval = Duration::from_micros(1_000_000 / u64::from(self.cfg.max_fps.max(1)));
        let mut inbuf = BytesMut::with_capacity(16 * 1024);
        loop {
            let mut throttle_until = None;
            if let Some(area) = self.due_area() {
                let region = self.pending.intersect_rect(&area);
                if !region.is_empty() {
                    let now = Instant::now();
                    let ready_at = self.last_update.map_or(now, |t| t + min_interval);
                    if ready_at <= now {
                        self.send_update(&mut wr, &region).await?;
                        self.pending = self.pending.subtract(&region);
                        self.requested = None;
                        self.last_update = Some(now);
                        continue;
                    }
                    throttle_until = Some(ready_at);
                }
            }

            if inbuf.capacity() - inbuf.len() < 1024 {
                inbuf.reserve(16 * 1024);
            }
            tokio::select! {
                n = rd.read_buf(&mut inbuf) => {
                    let n = n.context("read")?;
                    if n == 0 {
                        tracing::info!(peer = %self.peer, "client closed");
                        return Ok(());
                    }
                    while let Some((message, used)) = msg::parse_client(&inbuf)? {
                        inbuf.advance(used);
                        self.handle(message, &mut wr).await?;
                    }
                }
                ev = self.damage_rx.recv() => match ev {
                    Ok(ev) => self.pending = self.pending.union(&ev.region),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(peer = %self.peer, skipped = n, "behind on damage; whole picture pending");
                        let bounds = self.bounds();
                        self.pending = Region::from_rect(bounds);
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(throttle_until.unwrap_or_else(Instant::now))),
                    if throttle_until.is_some() => {}
            }
        }
    }

    async fn handle(&mut self, message: ClientMessage, wr: &mut OwnedWriteHalf) -> Result<()> {
        match message {
            ClientMessage::SetPixelFormat(pf) => {
                pf.validate()
                    .with_context(|| format!("client asked for {pf:?}"))?;
                tracing::debug!(peer = %self.peer, ?pf, "pixel format");
                self.pf = pf;
            }
            ClientMessage::SetEncodings(list) => {
                let names: Vec<&str> = list.iter().map(|&e| encoding::name(e)).collect();
                tracing::debug!(peer = %self.peer, ?names, "encodings");
                if list.contains(&encoding::PSEUDO_CONTINUOUS_UPDATES) && !self.continuous_offered {
                    // Support is announced the way TigerVNC's server does it:
                    // an EndOfContinuousUpdates before any were enabled.
                    self.out.clear();
                    msg::write_end_of_continuous_updates(&mut self.out);
                    wr.write_all(&self.out).await?;
                    self.continuous_offered = true;
                }
                self.encodings = list;
            }
            ClientMessage::FramebufferUpdateRequest {
                incremental,
                x,
                y,
                width,
                height,
            } => {
                let r = Rect::new(i32::from(x), i32::from(y), i32::from(width), i32::from(height))
                    .intersection(&self.bounds());
                if !incremental {
                    self.pending.add(r);
                }
                self.requested = Some(self.requested.map_or(r, |q| q.union_bounds(&r)));
            }
            ClientMessage::KeyEvent { down, keysym } | ClientMessage::QemuKeyEvent { down, keysym, .. } => {
                self.shared.input.lock().key(keysym, down);
            }
            ClientMessage::PointerEvent { buttons, x, y } => {
                self.shared.input.lock().pointer(x, y, buttons);
            }
            ClientMessage::ClientCutText(text) => {
                tracing::debug!(peer = %self.peer, len = text.len(), "cut text (clipboard not wired yet)");
            }
            ClientMessage::ExtendedClipboard(_) => {}
            ClientMessage::EnableContinuousUpdates {
                enable,
                x,
                y,
                width,
                height,
            } => {
                if enable {
                    let r = Rect::new(i32::from(x), i32::from(y), i32::from(width), i32::from(height));
                    self.continuous = Some(r.intersection(&self.bounds()));
                } else {
                    self.continuous = None;
                    self.out.clear();
                    msg::write_end_of_continuous_updates(&mut self.out);
                    wr.write_all(&self.out).await?;
                }
            }
            ClientMessage::ClientFence { flags, payload } => {
                // No ordering guarantees yet: answer at once with the
                // request bit cleared. Flow control (phase 2) makes this real.
                self.out.clear();
                msg::write_server_fence(&mut self.out, flags & !msg::FENCE_REQUEST, &payload);
                wr.write_all(&self.out).await?;
            }
            ClientMessage::SetDesktopSize { width, height, .. } => {
                tracing::debug!(peer = %self.peer, width, height, "SetDesktopSize ignored (resize not supported yet)");
            }
        }
        Ok(())
    }

    async fn send_update(&mut self, wr: &mut OwnedWriteHalf, region: &Region) -> Result<()> {
        self.out.clear();
        let rects = region.rects();
        {
            let fb = self.shared.fb.read();
            let n = u16::try_from(rects.len()).context("more rectangles than one update can carry")?;
            msg::write_framebuffer_update_header(&mut self.out, n);
            for r in rects {
                msg::write_rect_header(
                    &mut self.out,
                    r.x1 as u16,
                    r.y1 as u16,
                    r.width() as u16,
                    r.height() as u16,
                    encoding::RAW,
                );
                raw::encode(&fb, *r, &self.pf, &mut self.out);
            }
        }
        wr.write_all(&self.out).await.context("write update")?;
        tracing::trace!(peer = %self.peer, rects = rects.len(), bytes = self.out.len(), "update");
        Ok(())
    }
}
