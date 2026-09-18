//! One client, from handshake to hangup.
//!
//! The session keeps pending state of its own, fed by the capture thread's
//! frame events: damage, moves it can still send as CopyRect, a pointer
//! shape, a resize. It sends one FramebufferUpdate whenever the client has
//! asked for one (or is in continuous mode) and something in the asked-for
//! area is pending. Everything pending goes in one update, so a slow client
//! sees fewer and larger updates rather than a queue of stale ones.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use tokio::sync::{broadcast, mpsc};

use crate::flow::{Congestion, Limits};
use crate::shared::{FrameEvent, Shared};
use crate::stats::SessionStats;

/// Updates the writer may have queued before the session stops building
/// them. Small on purpose: this is the back-pressure. A deep queue would let
/// the session run ahead and fill it with updates that are stale by the time
/// they reach the socket, which is the exact fault the window exists to
/// prevent; a shallow one turns "the client is behind" into something the
/// session notices within a frame or two and answers by folding the next
/// frames into what it already owes rather than by queueing more.
const WRITER_QUEUE: usize = 8;

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
    /// Where a client's congestion window starts and how fast it opens.
    pub flow: Limits,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            password: None,
            max_fps: 60,
            auth_fail_delay: Duration::from_secs(1),
            flow: Limits::default(),
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
    let written = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel::<Vec<u8>>(WRITER_QUEUE);
    let writer = tokio::spawn(write_loop(wr, rx, written.clone()));
    let stats = SessionStats::new(peer);
    shared.register(stats.clone());
    let bounds = Rect::new(0, 0, width as i32, height as i32);
    let flow = Congestion::new(cfg.flow);
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
        tx,
        written,
        flow,
        outbox: VecDeque::new(),
        held_fence: None,
        reader_paused: false,
        newest_frame: None,
        stats,
    };
    let outcome = session.serve(rd).await;
    // The writer stops when its channel closes, which the session dropping
    // does; waiting for it means the last update is on the wire before the
    // socket goes.
    let _ = writer.await;
    outcome
}

/// The writer task: one socket, one queue, nothing else.
///
/// Writing from the task that reads the client is what makes a slow socket
/// look like a slow client. `write_all` on a full send buffer waits, and
/// while it waits nothing reads the client's messages or folds new frames
/// into what it owes, so the session goes deaf exactly when it is furthest
/// behind. Here the wait happens somewhere the rest of the session does not
/// care about.
async fn write_loop(mut wr: OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>, written: Arc<AtomicU64>) {
    while let Some(buf) = rx.recv().await {
        if let Err(e) = wr.write_all(&buf).await {
            let e = anyhow::Error::from(e);
            if !is_gone(&e) {
                tracing::warn!(error = %e, "write failed");
            }
            return;
        }
        written.fetch_add(buf.len() as u64, Ordering::Relaxed);
    }
    let _ = wr.shutdown().await;
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
    /// Ready bytes on their way to the socket. The writer owns the write
    /// half; nothing else in the session ever touches it, which is what
    /// keeps a stalled socket from stalling the reader with it.
    tx: mpsc::Sender<Vec<u8>>,
    /// Bytes the writer has actually put on the wire.
    written: Arc<AtomicU64>,
    flow: Congestion,
    /// Control messages waiting for room in the writer's queue: fence
    /// responses, the end of continuous updates, a refused resize.
    outbox: VecDeque<Vec<u8>>,
    /// A fence response held back for the next update, because the client
    /// asked for SyncNext.
    held_fence: Option<Vec<u8>>,
    /// The client asked for BlockAfter, so nothing more is read from it
    /// until its response has been handed over. The bytes wait in the
    /// socket's own buffer, which is where the kernel is happy to keep them.
    reader_paused: bool,
    /// When the newest frame folded into `pending` was captured, so an
    /// update can say how old the freshest thing in it is.
    newest_frame: Option<Instant>,
    stats: Arc<SessionStats>,
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
        // The freshest thing this client is owed. Its age when the update
        // goes out is what the latency histogram counts.
        self.newest_frame = Some(ev.at);
    }

    async fn serve(mut self, mut rd: OwnedReadHalf) -> Result<()> {
        let outcome = self.serve_loop(&mut rd).await;
        self.publish();
        // The counts go out whichever way the session ended: a clean close,
        // a reset when the viewer went away, or a protocol error.
        let s = &self.stats;
        tracing::info!(
            peer = %self.peer,
            updates = s.updates.load(Ordering::Relaxed),
            rects = s.rects.load(Ordering::Relaxed),
            copy_rects = s.copy_rects.load(Ordering::Relaxed),
            cursor_rects = s.cursor_rects.load(Ordering::Relaxed),
            bytes = s.bytes.load(Ordering::Relaxed),
            window = self.flow.window(),
            lost_pings = self.flow.lost,
            "session over"
        );
        self.shared.unregister(&self.stats);
        match outcome {
            Err(e) if is_gone(&e) => Ok(()),
            other => other,
        }
    }

    /// Copy what the stats endpoint reads out of the session's own state.
    /// Done once a turn rather than at every change, since nothing here is
    /// worth a lock and a reader wants a snapshot, not a stream.
    fn publish(&self) {
        let s = &self.stats;
        let micros = |d: Option<Duration>| d.map_or(0, |d| d.as_micros() as u64);
        s.bytes
            .store(self.written.load(Ordering::Relaxed), Ordering::Relaxed);
        s.in_flight.store(self.flow.in_flight(), Ordering::Relaxed);
        s.window.store(self.flow.window(), Ordering::Relaxed);
        s.pings_outstanding
            .store(self.flow.outstanding() as u64, Ordering::Relaxed);
        s.rtt_micros.store(micros(self.flow.rtt()), Ordering::Relaxed);
        s.base_rtt_micros
            .store(micros(self.flow.base_rtt()), Ordering::Relaxed);
    }

    async fn serve_loop(&mut self, rd: &mut OwnedReadHalf) -> Result<()> {
        let min_interval = Duration::from_micros(1_000_000 / u64::from(self.cfg.max_fps.max(1)));
        let mut inbuf = BytesMut::with_capacity(16 * 1024);
        // A handle of its own to wait for room on, so the wait does not hold
        // a borrow of the session that sending would need back.
        let tx = self.tx.clone();
        loop {
            let now = Instant::now();
            self.flow.expire(now);

            // Whatever the client has already sent is handled before
            // anything else, which is what makes a fence's BlockBefore free:
            // by the time its response is built, every message that arrived
            // in front of it has been through here.
            while !self.reader_paused {
                let Some((message, used)) = msg::parse_client(&inbuf)? else {
                    break;
                };
                inbuf.advance(used);
                self.handle(message)?;
            }

            // A response held for an update that can never come would leave
            // the client waiting for ever, with its own reader paused if it
            // asked for BlockAfter as well. Let it go instead.
            if self.held_fence.is_some() && self.due_area().is_none() {
                let fence = self.held_fence.take().expect("just checked");
                self.outbox.push_back(fence);
            }
            self.publish();

            let mut throttle_until = None;
            let mut want_send = !self.outbox.is_empty();
            if !want_send
                && let Some(area) = self.due_area()
                && self.has_something_for(area)
            {
                let ready_at = self.last_update.map_or(now, |t| t + min_interval);
                if ready_at > now {
                    throttle_until = Some(ready_at);
                } else if self.flow.may_send() {
                    want_send = true;
                }
                // Otherwise the window is shut. Nothing is built, the frames
                // arriving keep folding into what this client is owed, and
                // its next fence answer opens it again: one larger update
                // later beats two stale ones now.
            }

            if inbuf.capacity() - inbuf.len() < 1024 {
                inbuf.reserve(16 * 1024);
            }
            tokio::select! {
                permit = tx.reserve(), if want_send => {
                    let permit = permit.context("the writer stopped")?;
                    self.send_next(permit)?;
                }
                n = rd.read_buf(&mut inbuf), if !self.reader_paused => {
                    if n.context("read")? == 0 {
                        tracing::info!(peer = %self.peer, "client closed");
                        return Ok(());
                    }
                }
                ev = self.frames.recv() => match ev {
                    Ok(ev) => self.absorb(&ev),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(peer = %self.peer, skipped = n, "behind on frames; whole picture pending");
                        let bounds = self.bounds();
                        self.pending = Region::from_rect(bounds);
                        self.copies.clear();
                        self.newest_frame = Some(Instant::now());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(throttle_until.unwrap_or_else(Instant::now))),
                    if throttle_until.is_some() => {}
            }
        }
    }

    /// Hand one thing to the writer: a control message if any is waiting,
    /// otherwise an update.
    fn send_next(&mut self, permit: mpsc::Permit<'_, Vec<u8>>) -> Result<()> {
        if let Some(buf) = self.outbox.pop_front() {
            self.flow.wrote(buf.len() as u64);
            permit.send(buf);
            // Whatever the client was told to wait for has now gone.
            self.reader_paused = false;
            return Ok(());
        }
        let Some(area) = self.due_area() else {
            return Ok(());
        };
        if !self.build_update(area)? {
            return Ok(());
        }

        let now = Instant::now();
        let mut buf = Vec::with_capacity(self.out.len() + 96);
        if let Some(fence) = self.held_fence.take() {
            // Immediately before the update and in the same send, so nothing
            // can arrive between the two: that is what SyncNext asks for.
            buf.extend_from_slice(&fence);
            self.reader_paused = false;
        }
        buf.extend_from_slice(&self.out);
        self.flow.wrote(buf.len() as u64);
        if self.flow.ping_due(now) {
            // After the update, so the round trip covers the bytes just
            // queued: the answer says the client has been through all of it.
            let seq = self.flow.ping(now);
            msg::write_server_fence(
                &mut buf,
                msg::FENCE_REQUEST | msg::FENCE_BLOCK_BEFORE,
                &seq.to_be_bytes(),
            );
        }
        if let Some(at) = self.newest_frame.take() {
            self.stats.latency.record(now.saturating_duration_since(at));
        }
        self.last_update = Some(now);
        permit.send(buf);
        Ok(())
    }

    fn has_something_for(&self, area: Rect) -> bool {
        self.layout_pending
            || (self.cursor_pending.is_some() && self.cursor_encoding().is_some())
            || self.copies.iter().any(|c| c.dst.intersects(&area))
            || self.pending.intersects_rect(&area)
    }

    fn handle(&mut self, message: ClientMessage) -> Result<()> {
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
                    let mut buf = Vec::with_capacity(4);
                    msg::write_end_of_continuous_updates(&mut buf);
                    self.outbox.push_back(buf);
                    self.continuous_offered = true;
                }
                if list.contains(&encoding::PSEUDO_FENCE) {
                    // The client can answer a fence, so the link can be
                    // measured and a window is worth keeping.
                    self.flow.measure();
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
                    let mut buf = Vec::with_capacity(4);
                    msg::write_end_of_continuous_updates(&mut buf);
                    self.outbox.push_back(buf);
                }
            }
            ClientMessage::ClientFence { flags, payload } => {
                if flags & msg::FENCE_REQUEST == 0 {
                    // Not a request: this is the client answering one of our
                    // own pings, and the only fences we send carry an
                    // eight-byte sequence number. Anything else is some other
                    // server's idea and is left alone.
                    if let Ok(seq) = <[u8; 8]>::try_from(&payload[..]) {
                        let seq = u64::from_be_bytes(seq);
                        if self.flow.pong(seq, Instant::now()) {
                            tracing::debug!(
                                peer = %self.peer,
                                rtt = ?self.flow.rtt(),
                                base = ?self.flow.base_rtt(),
                                window = self.flow.window(),
                                in_flight = self.flow.in_flight(),
                                "fence answered"
                            );
                        }
                    }
                    return Ok(());
                }
                // A request. BlockBefore needs nothing done for it: messages
                // are handled in the order they arrived, so everything sent
                // before this fence has already been through by now.
                let mut response = Vec::with_capacity(payload.len() + 12);
                msg::write_server_fence(&mut response, flags & !msg::FENCE_REQUEST, &payload);
                if flags & msg::FENCE_BLOCK_AFTER != 0 {
                    // Nothing more is read from this client until the
                    // response has gone. Its bytes wait in the socket.
                    self.reader_paused = true;
                }
                if flags & msg::FENCE_SYNC_NEXT != 0 {
                    self.held_fence = Some(response);
                } else {
                    self.outbox.push_back(response);
                }
            }
            ClientMessage::SetDesktopSize { width, height, .. } => {
                // The picture is a real screen; a client cannot resize it.
                // Say so with the layout as it stands.
                tracing::debug!(peer = %self.peer, width, height, "SetDesktopSize refused: the screen is not resizable");
                self.out.clear();
                msg::write_framebuffer_update_header(&mut self.out, 1);
                self.write_layout_rect(resize_reason::THIS_CLIENT, resize_status::PROHIBITED);
                let refusal = std::mem::take(&mut self.out);
                self.outbox.push_back(refusal);
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
            self.stats.updates.fetch_add(1, Ordering::Relaxed);
            self.stats.rects.fetch_add(1, Ordering::Relaxed);
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
            self.stats.cursor_rects.fetch_add(1, Ordering::Relaxed);
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
        self.stats
            .copy_rects
            .fetch_add(sendable.len() as u64, Ordering::Relaxed);
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
        self.stats.updates.fetch_add(1, Ordering::Relaxed);
        self.stats.rects.fetch_add(n as u64, Ordering::Relaxed);
        tracing::trace!(peer = %self.peer, rects = n, copies = sendable.len(), bytes = self.out.len(), "update");
        Ok(true)
    }
}
