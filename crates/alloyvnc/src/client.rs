//! A small RFB client: enough to connect, ask for updates and decode Raw
//! and CopyRect into a framebuffer of its own.
//!
//! It exists for the tests and the measurement harness. It is not a viewer:
//! it draws nothing and speaks only 3.8 with the framebuffer's native pixel
//! format.

use std::net::SocketAddr;

use alloyvnc_encode::Framebuffer;
use alloyvnc_proto::handshake::{self, Flow, ServerInit, security};
use alloyvnc_proto::msg::{self, server_type};
use alloyvnc_proto::{PixelFormat, auth, encoding};
use alloyvnc_region::Rect;
use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
pub struct Client {
    stream: TcpStream,
    /// The picture as decoded so far.
    pub fb: Framebuffer,
    pub name: String,
    pub last_cut_text: Option<String>,
    /// Fences the server answered, newest last.
    pub fences: Vec<(u32, Vec<u8>)>,
    pub end_of_continuous_updates: u32,
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
            end_of_continuous_updates: 0,
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

    /// Read server messages until one FramebufferUpdate has been applied,
    /// and return its rectangles. Bells, cut text, fences and
    /// EndOfContinuousUpdates are recorded and skipped.
    pub async fn next_update(&mut self) -> Result<Vec<Rect>> {
        loop {
            let kind = self.stream.read_u8().await.context("server message")?;
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
                            encoding::PSEUDO_LAST_RECT => break,
                            other => bail!(
                                "encoding {other} ({}) not decoded by the test client",
                                encoding::name(other)
                            ),
                        }
                        rects.push(rect);
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
                    let mut payload = vec![0u8; len];
                    self.stream.read_exact(&mut payload).await?;
                    self.fences.push((flags, payload));
                }
                other => bail!("server message type {other} not handled by the test client"),
            }
        }
    }
}

async fn read_reason(stream: &mut TcpStream) -> Result<String> {
    let len = stream.read_u32().await? as usize;
    ensure!(len <= 4096, "reason of {len} bytes");
    let mut raw = vec![0u8; len];
    stream.read_exact(&mut raw).await?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}
