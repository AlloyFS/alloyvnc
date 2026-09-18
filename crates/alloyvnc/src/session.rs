//! One client, from handshake to hangup.
//!
//! The session keeps pending state of its own, fed by the capture thread's
//! frame events: damage, moves it can still send as CopyRect, a pointer
//! shape, a resize. It sends one FramebufferUpdate whenever the client has
//! asked for one (or is in continuous mode) and something in the asked-for
//! area is pending. Everything pending goes in one update, so a slow client
//! sees fewer and larger updates rather than a queue of stale ones.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloyvnc_encode::{CursorShape, copyrect, cursor, raw};
use alloyvnc_proto::handshake::{self, Flow, ServerInit, security};
use alloyvnc_proto::msg::{self, ClientMessage, Screen, resize_reason, resize_status};
use alloyvnc_proto::{PixelFormat, auth, encoding};
use alloyvnc_region::{Move, Rect, Region};
use anyhow::{Context, Result, bail};
use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast;

use crate::shared::{FrameEvent, Shared};

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

    let frames = shared.frames.subscribe();
    let (rd, wr) = stream.into_split();
    let bounds = Rect::new(0, 0, width as i32, height as i32);
    let session = Session {
        shared,
        cfg,
        peer,
        pf: PixelFormat::bgrx32(),
        encodings: Vec::new(),
        client_size: (width, height),
        pending: Region::from_rect(bounds),
        copies: Vec::new(),
        cursor_pending: None,
        layout_pending: false,
        requested: None,
        continuous: None,
        continuous_offered: false,
        last_update: None,
        frames,
        out,
        stats: Stats::default(),
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

/// A viewer that vanished mid-session (its window closed, its network
/// dropped) is the ordinary way a session ends, not a fault to warn about.
fn is_gone(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof
        )
    })
}

#[derive(Debug, Default)]
struct Stats {
    updates: u64,
    rects: u64,
    copy_rects: u64,
    cursor_rects: u64,
    bytes: u64,
}

struct Session {
    shared: Arc<Shared>,
    cfg: Arc<SessionConfig>,
    peer: SocketAddr,
    pf: PixelFormat,
    encodings: Vec<i32>,
    /// The size the client believes the picture has.
    client_size: (u32, u32),
    /// What has changed since this client last saw it.
    pending: Region,
    /// Moves the client can still reproduce with CopyRect, in order.
    copies: Vec<Move>,
    cursor_pending: Option<Arc<CursorShape>>,
    /// An ExtendedDesktopSize rectangle is owed: the client just asked for
    /// the layout, or the picture changed size.
    layout_pending: bool,
    /// The area the client's outstanding update requests cover.
    requested: Option<Rect>,
    /// The area continuous updates were enabled for.
    continuous: Option<Rect>,
    continuous_offered: bool,
    last_update: Option<Instant>,
    frames: broadcast::Receiver<FrameEvent>,
    out: Vec<u8>,
    stats: Stats,
}

impl Session {
    fn bounds(&self) -> Rect {
        self.shared.fb.read().bounds()
    }

    fn supports(&self, encoding: i32) -> bool {
        self.encodings.contains(&encoding)
    }

    /// The pseudo-encoding the pointer goes out as, if the client takes one.
    fn cursor_encoding(&self) -> Option<i32> {
        [encoding::PSEUDO_CURSOR_WITH_ALPHA, encoding::PSEUDO_CURSOR]
            .into_iter()
            .find(|&e| self.supports(e))
    }

    fn resize_encoding(&self) -> Option<i32> {
        [
            encoding::PSEUDO_EXTENDED_DESKTOP_SIZE,
            encoding::PSEUDO_DESKTOP_SIZE,
        ]
        .into_iter()
        .find(|&e| self.supports(e))
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

    /// Fold a frame's changes into what this client still has to see.
    fn absorb(&mut self, ev: &FrameEvent) {
        if ev.resized {
            self.pending = Region::from_rect(self.bounds());
            self.copies.clear();
            self.layout_pending = true;
        }
        let copy_rect = self.supports(encoding::COPY_RECT);
        for m in ev.moves.iter() {
            // A CopyRect reproduces the move only if the client's copy of
            // the source is current: nothing pending touches it, and no
            // earlier unsent copy involves it. Otherwise the destination is
            // plain damage, which is always right and merely bigger.
            let src = m.src();
            let clean = copy_rect
                && !self.pending.intersects_rect(&src)
                && !self.copies.iter().any(|c| {
                    c.dst.intersects(&src) || c.dst.intersects(&m.dst) || c.src().intersects(&m.dst)
                });
            if clean {
                self.pending.remove(m.dst);
                self.copies.push(*m);
            } else {
                self.pending.add(m.dst);
            }
        }
        self.pending = self.pending.union(&ev.damage);
        if let Some(shape) = &ev.cursor {
            self.cursor_pending = Some(shape.clone());
        }
    }

    async fn serve(mut self, mut rd: OwnedReadHalf, mut wr: OwnedWriteHalf) -> Result<()> {
        let outcome = self.serve_loop(&mut rd, &mut wr).await;
        // The counts go out whichever way the session ended: a clean close,
        // a reset when the viewer went away, or a protocol error.
        tracing::info!(peer = %self.peer, stats = ?self.stats, "session over");
        match outcome {
            Err(e) if is_gone(&e) => Ok(()),
            other => other,
        }
    }

    async fn serve_loop(&mut self, rd: &mut OwnedReadHalf, wr: &mut OwnedWriteHalf) -> Result<()> {
        let min_interval = Duration::from_micros(1_000_000 / u64::from(self.cfg.max_fps.max(1)));
        let mut inbuf = BytesMut::with_capacity(16 * 1024);
        loop {
            let mut throttle_until = None;
            if let Some(area) = self.due_area() {
                let now = Instant::now();
                let ready_at = self.last_update.map_or(now, |t| t + min_interval);
                if ready_at <= now {
                    if self.build_update(area)? {
                        wr.write_all(&self.out).await.context("write update")?;
                        self.last_update = Some(now);
                        continue;
                    }
                } else if self.has_something_for(area) {
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
                        self.handle(message, wr).await?;
                    }
                }
                ev = self.frames.recv() => match ev {
                    Ok(ev) => self.absorb(&ev),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(peer = %self.peer, skipped = n, "behind on frames; whole picture pending");
                        let bounds = self.bounds();
                        self.pending = Region::from_rect(bounds);
                        self.copies.clear();
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(throttle_until.unwrap_or_else(Instant::now))),
                    if throttle_until.is_some() => {}
            }
        }
    }

    fn has_something_for(&self, area: Rect) -> bool {
        self.layout_pending
            || (self.cursor_pending.is_some() && self.cursor_encoding().is_some())
            || self.copies.iter().any(|c| c.dst.intersects(&area))
            || self.pending.intersects_rect(&area)
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
                if list.contains(&encoding::PSEUDO_EXTENDED_DESKTOP_SIZE)
                    && !self.supports(encoding::PSEUDO_EXTENDED_DESKTOP_SIZE)
                {
                    // The layout goes out once the client can take it.
                    self.layout_pending = true;
                }
                self.encodings = list;
                if self.cursor_pending.is_none() && self.cursor_encoding().is_some() {
                    // A client that arrived after the last shape change
                    // still needs the pointer as it is now.
                    self.cursor_pending = self.shared.cursor.lock().clone();
                }
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
                // The picture is a real screen; a client cannot resize it.
                // Say so with the layout as it stands.
                tracing::debug!(peer = %self.peer, width, height, "SetDesktopSize refused: the screen is not resizable");
                self.out.clear();
                msg::write_framebuffer_update_header(&mut self.out, 1);
                self.write_layout_rect(resize_reason::THIS_CLIENT, resize_status::PROHIBITED);
                wr.write_all(&self.out).await?;
            }
        }
        Ok(())
    }

    fn screens(&self) -> Vec<Screen> {
        self.shared
            .screens
            .lock()
            .iter()
            .enumerate()
            .map(|(i, r)| Screen {
                id: i as u32 + 1,
                x: r.x1 as u16,
                y: r.y1 as u16,
                width: r.width() as u16,
                height: r.height() as u16,
                flags: 0,
            })
            .collect()
    }

    fn write_layout_rect(&mut self, reason: u16, status: u16) {
        let (w, h) = self.client_size;
        msg::write_rect_header(
            &mut self.out,
            reason,
            status,
            w as u16,
            h as u16,
            encoding::PSEUDO_EXTENDED_DESKTOP_SIZE,
        );
        let screens = self.screens();
        msg::write_extended_desktop_size(&mut self.out, &screens);
    }

    /// Assemble one FramebufferUpdate for `area` into `self.out`. `false`
    /// when there is nothing to send.
    fn build_update(&mut self, area: Rect) -> Result<bool> {
        self.out.clear();
        let bounds = self.bounds();
        let size = (bounds.width() as u32, bounds.height() as u32);

        // A resize goes out on its own: the client throws its picture away
        // and asks again, so nothing else in the update would survive.
        if size != self.client_size {
            let Some(enc) = self.resize_encoding() else {
                bail!("the picture changed size and the client cannot follow");
            };
            self.client_size = size;
            msg::write_framebuffer_update_header(&mut self.out, 1);
            if enc == encoding::PSEUDO_EXTENDED_DESKTOP_SIZE {
                self.write_layout_rect(resize_reason::SERVER, resize_status::OK);
            } else {
                msg::write_rect_header(
                    &mut self.out,
                    0,
                    0,
                    size.0 as u16,
                    size.1 as u16,
                    encoding::PSEUDO_DESKTOP_SIZE,
                );
            }
            self.layout_pending = false;
            self.pending = Region::from_rect(bounds);
            self.copies.clear();
            self.requested = None;
            self.stats.updates += 1;
            self.stats.rects += 1;
            self.stats.bytes += self.out.len() as u64;
            return Ok(true);
        }

        // Copies whose destination the client did not ask for cannot go
        // out as CopyRect now; they become damage for later.
        let copies: Vec<Move> = std::mem::take(&mut self.copies);
        let mut sendable = Vec::with_capacity(copies.len());
        for m in copies {
            if area.contains(&m.dst) {
                sendable.push(m);
            } else {
                self.pending.add(m.dst);
            }
        }
        let damage = self.pending.intersect_rect(&area);
        let cursor = if self.cursor_encoding().is_some() {
            self.cursor_pending.take()
        } else {
            None
        };
        let layout = self.layout_pending && self.supports(encoding::PSEUDO_EXTENDED_DESKTOP_SIZE);
        if sendable.is_empty() && damage.is_empty() && cursor.is_none() && !layout {
            return Ok(false);
        }

        // A region past what one update can carry is sent as its bounds.
        let damage_rects: Vec<Rect> = if damage.len() > 60_000 {
            vec![damage.bounds()]
        } else {
            damage.rects().to_vec()
        };
        let n = usize::from(layout) + usize::from(cursor.is_some()) + sendable.len() + damage_rects.len();
        msg::write_framebuffer_update_header(&mut self.out, u16::try_from(n).context("rectangle count")?);

        if layout {
            self.write_layout_rect(resize_reason::SERVER, resize_status::OK);
            self.layout_pending = false;
        }
        if let Some(shape) = cursor {
            let (w, h) = (shape.width as u16, shape.height as u16);
            match self.cursor_encoding() {
                Some(encoding::PSEUDO_CURSOR_WITH_ALPHA) => {
                    msg::write_rect_header(
                        &mut self.out,
                        shape.hot_x as u16,
                        shape.hot_y as u16,
                        w,
                        h,
                        encoding::PSEUDO_CURSOR_WITH_ALPHA,
                    );
                    cursor::encode_cursor_with_alpha(&shape, &mut self.out);
                }
                _ => {
                    msg::write_rect_header(
                        &mut self.out,
                        shape.hot_x as u16,
                        shape.hot_y as u16,
                        w,
                        h,
                        encoding::PSEUDO_CURSOR,
                    );
                    cursor::encode_cursor(&shape, &self.pf, &mut self.out);
                }
            }
            self.stats.cursor_rects += 1;
        }
        for m in &sendable {
            msg::write_rect_header(
                &mut self.out,
                m.dst.x1 as u16,
                m.dst.y1 as u16,
                m.dst.width() as u16,
                m.dst.height() as u16,
                encoding::COPY_RECT,
            );
            copyrect::encode(m.src_x as u16, m.src_y as u16, &mut self.out);
        }
        self.stats.copy_rects += sendable.len() as u64;
        {
            let fb = self.shared.fb.read();
            for r in &damage_rects {
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
        self.pending = self.pending.subtract(&damage);
        self.requested = None;
        self.stats.updates += 1;
        self.stats.rects += n as u64;
        self.stats.bytes += self.out.len() as u64;
        tracing::trace!(peer = %self.peer, rects = n, copies = sendable.len(), bytes = self.out.len(), "update");
        Ok(true)
    }
}
