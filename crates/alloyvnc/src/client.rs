//! A small RFB client: enough to connect, ask for updates and decode Raw,
//! CopyRect and the pseudo-rectangles into a framebuffer of its own.
//!
//! It exists for the tests and the measurement harness. It is not a viewer:
//! it draws nothing and speaks only 3.8 with the framebuffer's native pixel
//! format.

use std::net::SocketAddr;

use alloyvnc_encode::{CursorShape, Framebuffer};
use alloyvnc_proto::handshake::{self, Flow, ServerInit, security};
use alloyvnc_proto::msg::{self, Screen, server_type};
use alloyvnc_proto::{PixelFormat, auth, encoding};
use alloyvnc_region::Rect;
use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One rectangle of an update, as the client saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub rect: Rect,
    pub encoding: i32,
}

#[derive(Debug)]
pub struct Client {
    stream: TcpStream,
    /// The picture as decoded so far.
    pub fb: Framebuffer,
    pub name: String,
    pub last_cut_text: Option<String>,
    /// Fences the server sent, newest last.
    pub fences: Vec<(u32, Vec<u8>)>,
    /// Fences the server asked to have echoed, which is how it measures the
    /// link. A real viewer answers each one as soon as it has finished with
    /// everything in front of it.
    pub pings: u64,
    /// Hold the answers instead of sending them, which is how a test plays
    /// a client that has stopped keeping up: the server's window fills and
    /// it stops building updates.
    pub hold_fences: bool,
    held: Vec<(u32, Vec<u8>)>,
    pub end_of_continuous_updates: u32,
    /// The pointer as last sent, hidden or not.
    pub cursor: Option<CursorShape>,
    /// The screen layout as last announced.
    pub screens: Vec<Screen>,
    /// Resizes seen, with the reason and status fields.
    pub resizes: Vec<(u16, u16)>,
    /// Every server message's type byte, in the order it arrived. What a
    /// test needs to say "and nothing came between these two".
    pub order: Vec<u8>,
}

impl Client {
    pub async fn connect(addr: SocketAddr, password: Option<&str>) -> Result<Client> {
        let mut stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connect {addr}"))?;
        stream.set_nodelay(true)?;

        let mut version = [0u8; handshake::VERSION_LEN];
        stream.read_exact(&mut version).await.context("server version")?;
        let (major, minor) = handshake::parse_version(&version)?;
        ensure!(
            Flow::for_version(major, minor)? == Flow::V38,
            "server speaks {major}.{minor}, client only 3.8"
        );
        stream.write_all(&Flow::V38.version_bytes()).await?;

        let count = stream.read_u8().await.context("security type count")?;
        if count == 0 {
            bail!("server refused: {}", read_reason(&mut stream).await?);
        }
        let mut types = vec![0u8; usize::from(count)];
        stream.read_exact(&mut types).await?;
        let chosen = if types.contains(&security::NONE) {
            security::NONE
        } else if types.contains(&security::VNC_AUTH) {
            security::VNC_AUTH
        } else {
            bail!("no usable security type in {types:?}");
        };
        stream.write_u8(chosen).await?;
        if chosen == security::VNC_AUTH {
            let mut challenge = [0u8; auth::CHALLENGE_LEN];
            stream.read_exact(&mut challenge).await.context("challenge")?;
            let response = auth::expected_response(password.unwrap_or(""), &challenge);
            stream.write_all(&response).await?;
        }
        let result = stream.read_u32().await.context("security result")?;
        if result != 0 {
            bail!("server refused: {}", read_reason(&mut stream).await?);
        }

        stream.write_u8(1).await?; // ClientInit: shared
        let mut head = vec![0u8; 24];
        stream.read_exact(&mut head).await.context("server init")?;
        let name_len = u32::from_be_bytes([head[20], head[21], head[22], head[23]]) as usize;
        ensure!(
            name_len <= ServerInit::MAX_NAME,
            "desktop name of {name_len} bytes"
        );
        head.resize(24 + name_len, 0);
        stream.read_exact(&mut head[24..]).await.context("desktop name")?;
        let (init, _) = ServerInit::parse(&head)?.context("server init incomplete")?;
        ensure!(
            init.pixel_format == PixelFormat::bgrx32(),
            "server native format {:?}",
            init.pixel_format
        );

        Ok(Client {
            stream,
            fb: Framebuffer::new(u32::from(init.width), u32::from(init.height)),
            name: init.name,
            last_cut_text: None,
            fences: Vec::new(),
            pings: 0,
            hold_fences: false,
            held: Vec::new(),
            end_of_continuous_updates: 0,
            cursor: None,
            screens: Vec::new(),
            resizes: Vec::new(),
            order: Vec::new(),
        })
    }

    pub async fn set_encodings(&mut self, encodings: &[i32]) -> Result<()> {
        let mut out = Vec::new();
        msg::write_set_encodings(&mut out, encodings);
        Ok(self.stream.write_all(&out).await?)
    }

    pub async fn request(&mut self, incremental: bool, rect: Rect) -> Result<()> {
        let mut out = Vec::new();
        msg::write_framebuffer_update_request(
            &mut out,
            incremental,
            rect.x1 as u16,
            rect.y1 as u16,
            rect.width() as u16,
            rect.height() as u16,
        );
        Ok(self.stream.write_all(&out).await?)
    }

    pub async fn request_all(&mut self, incremental: bool) -> Result<()> {
        let all = self.fb.bounds();
        self.request(incremental, all).await
    }

    pub async fn enable_continuous_updates(&mut self, enable: bool) -> Result<()> {
        let all = self.fb.bounds();
        let mut out = Vec::new();
        msg::write_enable_continuous_updates(&mut out, enable, 0, 0, all.width() as u16, all.height() as u16);
        Ok(self.stream.write_all(&out).await?)
    }

    pub async fn key(&mut self, keysym: u32, down: bool) -> Result<()> {
        let mut out = Vec::new();
        msg::write_key_event(&mut out, down, keysym);
        Ok(self.stream.write_all(&out).await?)
    }

    pub async fn pointer(&mut self, x: u16, y: u16, buttons: u8) -> Result<()> {
        let mut out = Vec::new();
        msg::write_pointer_event(&mut out, buttons, x, y);
        Ok(self.stream.write_all(&out).await?)
    }

    pub async fn fence(&mut self, flags: u32, payload: &[u8]) -> Result<()> {
        let mut out = Vec::new();
        msg::write_client_fence(&mut out, flags, payload);
        Ok(self.stream.write_all(&out).await?)
    }

    pub async fn set_desktop_size(&mut self, width: u16, height: u16) -> Result<()> {
        let mut out = vec![msg::client_type::SET_DESKTOP_SIZE, 0];
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[1, 0]);
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        Ok(self.stream.write_all(&out).await?)
    }

    /// Read server messages until one FramebufferUpdate has been applied,
    /// and return its rectangles in order. Bells, cut text, fences and
    /// EndOfContinuousUpdates are recorded and skipped.
    pub async fn next_update(&mut self) -> Result<Vec<Received>> {
        loop {
            let kind = self.stream.read_u8().await.context("server message")?;
            self.order.push(kind);
            match kind {
                server_type::FRAMEBUFFER_UPDATE => {
                    self.stream.read_u8().await?;
                    let n = self.stream.read_u16().await?;
                    let mut rects = Vec::with_capacity(usize::from(n));
                    for _ in 0..n {
                        let x = u32::from(self.stream.read_u16().await?);
                        let y = u32::from(self.stream.read_u16().await?);
                        let w = u32::from(self.stream.read_u16().await?);
                        let h = u32::from(self.stream.read_u16().await?);
                        let enc = self.stream.read_i32().await?;
                        let rect = Rect::new(x as i32, y as i32, w as i32, h as i32);
                        match enc {
                            encoding::RAW => {
                                let mut pixels = vec![0u8; (w * h) as usize * Framebuffer::BYTES_PER_PIXEL];
                                self.stream.read_exact(&mut pixels).await.context("raw pixels")?;
                                ensure!(
                                    self.fb.bounds().contains(&rect),
                                    "rect {rect:?} outside the picture"
                                );
                                self.fb.blit(x, y, w, h, &pixels);
                            }
                            encoding::COPY_RECT => {
                                let sx = u32::from(self.stream.read_u16().await?);
                                let sy = u32::from(self.stream.read_u16().await?);
                                self.fb.copy_within(sx, sy, rect);
                            }
                            encoding::PSEUDO_CURSOR => {
                                let mut pixels = vec![0u8; (w * h) as usize * 4];
                                self.stream.read_exact(&mut pixels).await?;
                                let mut mask = vec![0u8; (w as usize).div_ceil(8) * h as usize];
                                self.stream.read_exact(&mut mask).await?;
                                let stride = (w as usize).div_ceil(8);
                                let mut rgba = Vec::with_capacity(pixels.len());
                                for (i, px) in pixels.as_chunks::<4>().0.iter().enumerate() {
                                    let (cx, cy) = (i % w as usize, i / w as usize);
                                    let on = mask[cy * stride + cx / 8] & (0x80 >> (cx % 8)) != 0;
                                    rgba.extend_from_slice(&[px[2], px[1], px[0], if on { 255 } else { 0 }]);
                                }
                                self.cursor = Some(CursorShape::new(w, h, x, y, rgba));
                            }
                            encoding::PSEUDO_CURSOR_WITH_ALPHA => {
                                let inner = self.stream.read_i32().await?;
                                ensure!(inner == encoding::RAW, "cursor with alpha in encoding {inner}");
                                let mut rgba = vec![0u8; (w * h) as usize * 4];
                                self.stream.read_exact(&mut rgba).await?;
                                self.cursor = Some(CursorShape::new(w, h, x, y, rgba));
                            }
                            encoding::PSEUDO_DESKTOP_SIZE => {
                                self.fb.resize(w, h);
                                self.resizes.push((0, 0));
                            }
                            encoding::PSEUDO_EXTENDED_DESKTOP_SIZE => {
                                let mut head = [0u8; 4];
                                self.stream.read_exact(&mut head).await?;
                                let mut buf = head.to_vec();
                                buf.resize(4 + usize::from(head[0]) * 16, 0);
                                self.stream.read_exact(&mut buf[4..]).await?;
                                let (screens, _) =
                                    msg::parse_extended_desktop_size(&buf)?.context("screen list")?;
                                self.screens = screens;
                                self.resizes.push((x as u16, y as u16));
                                if (x as u16) != msg::resize_reason::THIS_CLIENT
                                    || (y as u16) == msg::resize_status::OK
                                {
                                    self.fb.resize(w, h);
                                }
                            }
                            encoding::PSEUDO_LAST_RECT => break,
                            other => bail!(
                                "encoding {other} ({}) not decoded by the test client",
                                encoding::name(other)
                            ),
                        }
                        rects.push(Received { rect, encoding: enc });
                    }
                    return Ok(rects);
                }
                server_type::BELL => {}
                server_type::SERVER_CUT_TEXT => {
                    let mut pad = [0u8; 3];
                    self.stream.read_exact(&mut pad).await?;
                    let len = self.stream.read_u32().await? as usize;
                    ensure!(len <= msg::MAX_CUT_TEXT, "cut text of {len} bytes");
                    let mut raw = vec![0u8; len];
                    self.stream.read_exact(&mut raw).await?;
                    self.last_cut_text = Some(msg::latin1_to_string(&raw));
                }
                server_type::END_OF_CONTINUOUS_UPDATES => self.end_of_continuous_updates += 1,
                server_type::SERVER_FENCE => {
                    let mut pad = [0u8; 3];
                    self.stream.read_exact(&mut pad).await?;
                    let flags = self.stream.read_u32().await?;
                    let len = usize::from(self.stream.read_u8().await?);
                    ensure!(len <= msg::MAX_FENCE_PAYLOAD, "fence payload of {len} bytes");
                    let mut payload = vec![0u8; len];
                    self.stream.read_exact(&mut payload).await?;
                    self.fences.push((flags, payload.clone()));
                    if flags & msg::FENCE_REQUEST != 0 {
                        self.pings += 1;
                        if self.hold_fences {
                            self.held.push((flags, payload));
                        } else {
                            self.echo(flags, &payload).await?;
                        }
                    }
                }
                other => bail!("server message type {other} not handled by the test client"),
            }
        }
    }

    /// Answer one fence: the same payload, the same flags without the
    /// request bit. A client that does not do this is telling the server
    /// nothing, and the server's window shuts on it.
    async fn echo(&mut self, flags: u32, payload: &[u8]) -> Result<()> {
        let mut out = Vec::with_capacity(payload.len() + 12);
        msg::write_client_fence(&mut out, flags & !msg::FENCE_REQUEST, payload);
        self.stream.write_all(&out).await?;
        Ok(())
    }

    /// Send every answer that was held back, oldest first.
    pub async fn release_fences(&mut self) -> Result<usize> {
        let held = std::mem::take(&mut self.held);
        let n = held.len();
        for (flags, payload) in held {
            self.echo(flags, &payload).await?;
        }
        Ok(n)
    }

    pub fn held_fences(&self) -> usize {
        self.held.len()
    }
}

async fn read_reason(stream: &mut TcpStream) -> Result<String> {
    let len = stream.read_u32().await? as usize;
    ensure!(len <= 4096, "reason of {len} bytes");
    let mut raw = vec![0u8; len];
    stream.read_exact(&mut raw).await?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}
