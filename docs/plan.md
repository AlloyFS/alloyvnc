# AlloyVNC: the plan

Written 2026-09-18. Scope: a VNC server (RFB 3.8, RFC 6143) that exports a
live desktop, Windows first and Linux second, built for low latency and few
bytes rather than for feature count. Every claim of speed below is a number
to be taken on the development laptop (i5-6200U, 2C/4T, HD 520) with the
harness in section 6, before and after adjacently.

Status lives in the backlog, not here. Phase 0 landed the day this was
written.

## 1. What "high performance" means here

| Scenario | Target |
|---|---|
| Idle desktop | 0 bytes on the wire; the capture thread parked inside the OS call; under 1% CPU |
| Typing in a terminal | change on screen to bytes on the socket under 5 ms; end to end one frame plus RTT |
| Scrolling a page | the moved body as one CopyRect plus the exposed strip; tens of bytes for the move |
| 1080p video window | 30 fps sustained at the client's quality on one core with Tight + JPEG; H.264 later |
| Window drag | CopyRect for the body where the OS reports a move; damage only for the trail |
| Two clients, one slow | the fast one keeps its rate; the slow one gets fewer, larger updates, never a backlog |

Baselines on the same scenarios: TightVNC and UltraVNC on Windows (both poll
with GDI or hooks, which is why every Windows VNC server feels slow), x11vnc on
Linux. TigerVNC is the reference for wire behaviour: its viewer is the
strictest client, and its congestion control is the one to copy.

## 2. What already exists (checked 2026-09-18)

| Crate | Covers | Misses |
|---|---|---|
| rustvncserver 2.0, Apache-2.0 | protocol on tokio; every standard encoding through rfb-encodings; persistent zlib streams | capture; Cursor pseudo-encoding; ContinuousUpdates; Fence; ExtendedDesktopSize; TLS; WebSocket |
| rfb-encodings 0.1.7, Apache-2.0 | Raw, RRE, CoRRE, Hextile, Zlib, Tight, TightPng, ZRLE, ZYWRLE; pure Rust; flate2; optional turbojpeg | SIMD; benchmarks |
| vncrs 0.1.8, MIT | Windows only; Windows.Graphics.Capture with DWM damage regions; SIMD diff; Tight, ZRLE, Hextile | Linux; move rects (WGC has none); cursor shapes; flow control |
| rfb (Oxide) | server trait on tokio for VM consoles | everything desktop-side |
| dxgi-capture-rs, dxgcap, scrap (RustDesk) | DXGI duplication wrappers | dirty rects, move rects and the pointer shape are dropped or partial |

Decision: own the protocol layer. It is small, and it is where flow control and
the pseudo-encodings live, which no crate has. Take rfb-encodings for Hextile,
ZRLE and Tight as the phase-3 baseline, profile, and replace an encoder only
where the profile points. Write the capture backends against the OS APIs
directly (the `windows-sys` and `x11rb` crates): the dirty and move rects are
the whole point, and the wrappers drop them.

## 3. Architecture

```
capture thread (std, blocking OS call)
    | dirty rects, move rects, pointer shape, pixels
    v
framebuffer (BGRX, stride) + frame counter
    | per-client pending Region, pending CopyRects
    v
scheduler (per client) --> encoder (rayon pool) --> tokio writer task --> socket
                                                     tokio reader task <-- socket
                                                        | key, pointer, cut text
                                                        v
                                                   input thread (SendInput / XTest)
```

**Threads.** Capture is a plain std thread: DXGI's `AcquireNextFrame` and the
X11 event wait both block, and a blocked tokio worker stalls every socket.
Encoding runs on a rayon pool of N-1 threads. Tokio's multi-thread runtime with
two workers owns the sockets. Bounded channels between all three, so a stalled
consumer applies back-pressure instead of growing a queue.

**Framebuffer.** One buffer in the capture's native format (BGRX 8888 with a
row stride) behind a `parking_lot::RwLock`. The write lock is held only for the
blit of the dirty rects; encoders hold the read lock for the encode of one rect.
A monotonic frame counter records what changed when.

**Damage.** Two layers. First what the OS says: DXGI dirty rects and move
rects on Windows, XDamage on X11. Second a compare pass over 64x64 tiles hashed
with xxh3 against the previous frame's hashes. The compare tightens a large OS
rect (a whole window redrawn with one pixel changed) and is the only source of
damage for backends that report none (the synthetic one, a GDI fallback). At
xxh3's rate a full 1080p frame costs about half a millisecond, so it runs on
every frame.

**Region algebra.** Union, subtract and intersect over y-x banded rectangle
lists, about 300 lines, tested against a bitmap oracle with random operations.
Everything downstream is a Region: pending damage per client, CopyRect
validity, the rect list of an update.

**Per-client scheduler.** Each client owns: a pending damage Region, a pending
CopyRect list, its pixel format, its encoding list, quality and compression
levels, its zlib streams, an in-flight byte count. An update goes out when the
client has asked (FramebufferUpdateRequest) or is in continuous mode, there is
damage, and the flow-control window has room. One FramebufferUpdate per turn
carrying everything pending, so a slow client sees fewer and larger updates
rather than a queue of stale ones. A minimum interval caps the rate at 60 Hz.

**CopyRect from move rects.** A move goes out as CopyRect only when the
client's view of the source area is current (no pending damage intersects it);
otherwise the destination becomes damage. That ordering is TigerVNC's rule and
the thing naive servers get wrong, seen as ghosting during a scroll.

**Flow control.** Fence (pseudo-encoding -312, message 248) round trips give
the RTT; bytes in flight stay under the estimated bandwidth-delay product, as
TigerVNC's congestion control does. Without it a LAN client is fine and a WAN
client falls seconds behind. ContinuousUpdates (-313, message 150) turns the
protocol from pull to push for the clients that support it (TigerVNC, noVNC).

**Encoders.** Raw and CopyRect first. Then Hextile (16x16 tiles), ZRLE (64x64
tiles, palette RLE, one zlib stream), Tight (solid fill, palette, gradient
filter, zlib streams 0..3, JPEG for photographic sub-rects chosen by colour
count). Every zlib stream is per client and persistent: the client's inflater
mirrors the server's deflater state byte for byte, so encoded output cannot be
shared between clients and the zlib must be a streaming one (flate2 with the
zlib-rs backend; libdeflate is stateless and cannot be used here). Parallel
encoding: Tight's four streams allow up to four stripes of one rect in flight
on separate threads. Measured before kept, since this box has two cores and the
desktop being captured is using one of them.

**Pixel formats.** A client asking for the framebuffer's own layout gets its
bytes untouched. Everything else goes through one conversion loop written so
the compiler vectorises it (AVX2 here). See the findings: noVNC asks for
RGBX, which is a byte swizzle of the framebuffer, not a copy.

**Cursor.** Never composited into the framebuffer. DXGI's pointer shape
(monochrome, colour, masked colour) and XFixes' cursor image become the Cursor
(-239) or CursorWithAlpha (-314) pseudo-encoding; the position goes as
PointerPos (-232) only when the server side moves it.

**Input.** A KeyEvent carries an X11 keysym. Windows: characters map through
`VkKeyScanW` to a virtual key plus a shift state, specials through a table,
anything not on the active layout through `KEYEVENTF_UNICODE`; `SendInput` for
all of it. Pointer: `MOUSEEVENTF_ABSOLUTE` over the virtual screen; a wheel
tick is the press half of the button 4/5 pair. Linux: XTest. QEMU Extended Key
Event (-258) for raw scancodes later.

**Clipboard.** ClientCutText and ServerCutText (Latin-1) first; Extended
Clipboard (UTF-8, pseudo-encoding 0xc0a1e5ce) later; a clipboard listener on
the OS side.

**Resolution and monitors.** ExtendedDesktopSize (-308) with one screen per
output, outputs tiled into one framebuffer. DXGI's `ACCESS_LOST` on a mode
change rebuilds the duplication and announces the new size.

**Transport.** TCP on 5900 plus display number, `TCP_NODELAY`, vectored
writes of header and payload, a large write buffer. WebSocket (binary) for
noVNC through tokio-tungstenite on its own port, with a minimal static
responder for the noVNC bundle, served from a directory beside the binary.
TLS through rustls: VeNCrypt (security type 19, X509Plain) for native
clients, wss for the browser.

**Security defaults.** Loopback bind unless told otherwise. The None security
type refused on a non-loopback bind. VNC Auth (DES, eight characters) allowed
with a logged warning; VeNCrypt recommended. Auth failures rate-limited per
address.

## 4. Crate layout

A Cargo workspace at `C:\WebDAV\SHARED\alloyvnc`, repository
`AlloyFS/alloyvnc`, MIT like alloyfs:

| Crate | Holds | Testable where |
|---|---|---|
| alloyvnc-proto | messages, pixel format, encoding registry, handshake, VNC auth; no I/O | anywhere; fuzz targets |
| alloyvnc-region | region algebra | anywhere; oracle tests |
| alloyvnc-encode | the framebuffer, pixel conversion, encoders, rect splitting | anywhere; criterion benches |
| alloyvnc-screen | Capture and Input traits; the synthetic screen | anywhere |
| alloyvnc-screen-dxgi | Windows capture and SendInput | this laptop |
| alloyvnc-screen-x11 | XShm, XDamage, XFixes, XTest | WSLg, Xvfb on azure |
| alloyvnc | the server, its sessions, a test client, the binary | both |

The pure crates carry the tests CI runs on both OSes; the platform crates carry
the unsafe.

## 5. Phases, each behind a measurement

0. **Skeleton.** Workspace, CI on Windows and Ubuntu, synthetic screen (a
   deterministic animated pattern with replayable scenarios), handshake, Raw,
   one client. Gate: a real viewer shows the pattern; the parser has tests
   for every message and its prefixes.
1. **Windows capture.** DXGI duplication: dirty rects, move rects to CopyRect,
   pointer shape to cursor, `ACCESS_LOST` recovery, multi-output. Gate: idle
   under 1% CPU; a scroll sends CopyRect; bytes per scenario recorded.
2. **Damage, scheduler, flow control.** Compare pass, per-client pending,
   a writer task, real Fence ordering, congestion window. Gate: a latency
   histogram per scenario; a throttled client beside a fast one.
3. **Encoders.** Hextile, ZRLE, Tight with JPEG through rfb-encodings, pixel
   conversion; profile; replace what the profile shows. Gate: bytes and ms per
   scenario against TightVNC and UltraVNC on the same recording.
4. **Input and clipboard.** Keysym mapping, SendInput, wheel, clipboard both
   ways. Gate: every key of a US layout round-trips through the rig, and a
   character off the layout arrives.
5. **Browser path.** WebSocket, the noVNC bundle, TLS. Gate: noVNC in the rig
   at 1080p, fps and latency from the rig's scripts, no websockify.
6. **Linux.** The X11 backend against WSLg's XWayland and Xvfb on azure. Gate:
   the same scenarios.
7. **Hardening and reach.** VeNCrypt; service mode on Windows (logon screen,
   UAC, the secure desktop; alloyfs's `service/spawn.rs` already does the
   session handoff); Wayland through PipeWire and the RemoteDesktop portal;
   H.264 (openh264 first, then Media Foundation for Quick Sync on the HD 520;
   noVNC and TigerVNC's viewer decode Open H.264, encoding 50); Extended
   Clipboard; QEMU key events. Each behind its own number.

## 6. Measurement harness

- Scenarios as recorded frame sequences the synthetic screen replays: typing,
  page scroll, video, window drag, idle. Same input every run, so numbers
  compare across days, and still taken adjacently per the benchmarking rule.
- The test client in `alloyvnc::client`, which decodes into a framebuffer of
  its own and can be compared byte for byte with the server's.
- Per-update counters on a local stats endpoint: capture to socket in
  microseconds, bytes, encode ms, rect count, CopyRect count, in-flight bytes.
- criterion benches for each encoder on fixed inputs.
- noVNC in the Chrome rig through cdp.ts for the real browser: fps from
  requestAnimationFrame, latency by flipping a pixel and reading it back.

## 7. Traps known up front

- DXGI: a frame arrives only when something changed, so a timeout is not an
  error. `ACCESS_LOST` on a mode change, a UAC prompt or the lock screen. The
  secure desktop cannot be captured unelevated. Each output is its own
  duplication. A frame not released promptly folds its dirty rects into the
  next one, so nothing is lost by running slow. WGC has no move rects, which is
  why DXGI and not WGC.
- `SendInput` is blocked by UIPI when the foreground window runs at a higher
  integrity level: an elevated app ignores an unelevated server's input.
  Service mode fixes it, later.
- Keysyms: Shift down followed by keysym 'A' arrives already shifted; do not
  press Shift again. Dead keys and AltGr go through the unicode path.
- Tight and ZRLE need streaming zlib: flate2 with zlib-rs, never libdeflate.
- turbojpeg needs cmake and nasm under llvm-mingw; jpeg-encoder (pure Rust) is
  the fallback. Measure both.
- Nothing blocking on a tokio worker: capture and input injection on their own
  threads.
- Two cores: the server competes with the desktop it captures. Every number
  with the CPU control alongside.
- Never bind 0.0.0.0 with security None. It is the first thing a scanner tries.

## 8. Decisions taken

- Name: alloyvnc, at `C:\WebDAV\SHARED\alloyvnc`, GitHub `AlloyFS/alloyvnc`.
- Windows first, Linux X11 second.
- Service mode (logon screen, UAC) is in scope; it needs an elevated install
  when its phase comes.
- MIT, the same line as alloyfs.
- noVNC is served from a directory beside the binary, not embedded.

## 9. Encoding speed, in order of payoff

Where a 1080p frame's time goes on one core of this laptop. Estimates, to be
replaced by the harness's numbers before any of the ordering below is trusted.

| Step | Full 1920x1080 frame | Note |
|---|---|---|
| Reading the pixels | 0.4 ms | 8 MB at memory bandwidth; never the bottleneck |
| Tile compare | 0.5 ms | xxh3 or memcmp; the same bytes read once more |
| Raw | 1 ms | a memcpy; 8 MB on the wire |
| Hextile | 5 to 10 ms | no zlib |
| JPEG, libjpeg-turbo, 4:2:0, q 80 | 15 to 25 ms | SIMD; 100 to 200 KB out |
| JPEG, pure Rust jpeg-encoder | 60 to 100 ms | no SIMD |
| zlib level 1 (Tight, ZRLE) | 60 to 100 ms | zlib-rs; 100 to 150 MB/s |
| zlib level 6 | 150 to 250 ms | the default nobody should use for a live picture |
| H.264, Quick Sync | under 5 ms of CPU | the GPU does it; 1080p60 sustained |

Deflate is an order of magnitude slower than every other step, and it is
where every "zlib" encoder spends its time. The order below follows from that.

1. **Encode nothing that did not change, and nothing the client cannot
   drain.** Tightened damage, CopyRect for moves, one update per client per
   turn: a 15 fps client costs 15 encodes a second while the screen changes
   at 60. Scheduling, not encoding, and the largest saving.
2. **Solid fill before anything else.** One compare per pixel against the
   first; four bytes out.
3. **JPEG for photographic content, zlib level 1 for the rest.** TurboVNC's
   finding, and it still holds. Colour count with early exits (stop at two,
   stop past 256); many-colour rects to JPEG; palette and text areas on zlib
   at level 1, about ten percent bigger than level 6 and three times faster.
   Chroma 4:2:0 at the low quality levels, 4:4:4 near the top where coloured
   text must stay sharp.
4. **libjpeg-turbo, not a pure-Rust JPEG.** The SIMD paths are 4 to 6x. The
   build cost under llvm-mingw (cmake, nasm) is paid once; measured against
   jpeg-encoder before the dependency is kept.
5. **SIMD in the pixel loops:** compare, solid detection, pixel-format
   conversion, palette lookup. Plain loops over `chunks_exact` first so the
   compiler vectorises them, checked in the assembly (`cargo asm`), then
   `core::arch` AVX2 with runtime dispatch (`is_x86_feature_detected!` and
   `#[target_feature]`, or the `pulp` crate) only for the loops the compiler
   missed. `std::simd` is nightly-only and stays out.
6. **Stripes across threads.** A big rect splits by rows. JPEG, Raw and
   Hextile stripes are independent; Tight stripes each take one of the four
   stream ids; ZRLE cannot split at all (one stream). On this box the gain is
   capped by the two cores the desktop shares; on a server it is near linear
   for JPEG. Measured before kept.
7. **No copies on the way out.** Every encoder writes into the update's
   outgoing buffer (turbojpeg takes a caller buffer; zlib-rs deflates into a
   slice); buffers pooled and reused; header and payload in one vectored
   write. mimalloc as the global allocator on Windows, where the default heap
   is slow; measured.
8. **Stateless output shared between clients.** JPEG, Raw and Hextile rects do
   not depend on stream state, so one encode of a rect serves every client on
   the same pixel format and quality. Zlib rects cannot be shared: each
   client's inflater mirrors the server's deflater state. Nothing at one
   client; ten viewers of one screen cost one encode.
9. **H.264 on the GPU for motion.** Quick Sync on the HD 520 through a Media
   Foundation encoder MFT (BGRA in, Annex B out). The Open H.264 encoding (50)
   keeps a context per rect geometry, so P-frames are allowed and the
   all-I-frame criticism of earlier attempts does not apply. noVNC (WebCodecs)
   and TigerVNC's viewer decode it. The one path that makes a video window
   cheap in CPU and bytes at once; behind its own number.
10. **Compare on the GPU.** With DXGI the frame is already a texture; a
    compute shader can hash tiles and pack only the changed ones for readback,
    so the CPU never touches unchanged pixels. Earned only if the CPU
    compare's 0.5 ms shows in a profile, which at 1080p it will not.

Not the default: ZRLE. Palette RLE plus zlib with no JPEG escape makes it the
slowest common encoder; Tight with JPEG covers the same clients and is faster
on every scenario.

## 10. Findings

Things measured or observed that changed the plan, newest last.

- **2026-09-18, noVNC's pixel format is RGBX, not BGRX.** Its SetPixelFormat
  asks for 32 bits per pixel, little-endian, red shift 0, green 8, blue 16:
  the red byte first. The framebuffer holds blue first, so every noVNC pixel
  goes through the generic packer rather than the copy path. A 32-bit
  swizzle fast path (one shuffle per four pixels under SSSE3) is the fix,
  behind a measurement in phase 3. TigerVNC's viewer is still to be checked.
- **2026-09-18, noVNC's encoding list**, in its order of preference: CopyRect,
  OpenH264, Tight, TightPNG, ZRLE, JPEG (21), Hextile, RRE, Zlib, Raw, then
  the pseudo-encodings TightQuality, TightCompression, DesktopSize, LastRect,
  QemuExtendedKey, DesktopName, ExtendedDesktopSize, xvp, Fence,
  ContinuousUpdates, ExtendedMouseButtons, ExtendedClipboard, VMwareCursor,
  Cursor. Open H.264 above Tight is what makes phase 7's encoder worth it in
  the browser.
