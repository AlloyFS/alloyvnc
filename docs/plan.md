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
6. **Linux.** The X11 backend against Xvfb, here in WSL and on azure; WSLg's
   own display cannot be captured (see the findings). Gate: the same
   scenarios.
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
- **2026-09-18, DXGI reports no move rectangles on Windows 11.** A window
  drag by hand, a programmatic SetWindowPos walk and a forty-page Notepad
  scroll all arrived as dirty rectangles: 0 moves in 2829 frames. The
  compositor repaints rather than blits, so GetFrameMoveRects stays empty.
  The move path is implemented and its arithmetic tested, but CopyRect on
  this OS has to come from the compare pass of phase 2, which can detect a
  vertical shift by hashing rows. The phase 1 gate "a scroll sends CopyRect"
  is not met by DXGI metadata alone.
- **2026-09-18, DuplicateOutput is refused now and then on this laptop.**
  E_ACCESSDENIED from a healthy desktop, five times in four minutes, then
  success with nothing changed. Two VNC servers run on this laptop as
  services: TightVNC Server, which listens on 5900 on every interface, and
  RealVNC Server, which connects through RealVNC's own cloud side. A more
  privileged process holding the desktop image is what that error means,
  and either could be the holder; not proven, since both were running
  during the successful runs too, and a later refusal came with no RealVNC
  client connected at all. The start retries five times, 400 ms apart.
  TightVNC on 5900 is why the tests and the rig use 5901, and it is the
  phase 3 baseline, already installed.
- **2026-09-18, phase 1 numbers.** Release build, adjacent runs, this
  laptop, the desktop busy (this app redrawing), noVNC in a hidden tab, Raw
  only: 2.0% of one core and 30.8 MB with no client; 4.7% and 38.8 MB with
  one, 237 updates, 343 rectangles and 190 MB in 22 s. The same run in a
  debug build read 60%: a number from a debug build says nothing about the
  code. The bytes are Raw's (8.6 MB/s) and the rectangles are DXGI's, a
  whole window per change; phase 3 and phase 2 respectively.
- **2026-09-18, a fronted viewer feeds the picture into itself.** With the
  noVNC tab visible on the captured desktop every update changes the screen
  and produces the next. Measurements keep the tab hidden: `cdp.ts open`
  fronts a tab, `Target.createTarget` with `background: true` does not.
- **2026-09-18, WSLg's display cannot be captured by anyone.** XWayland runs
  rootless there: client windows are Wayland surfaces, the root window holds
  no pixels and receives no damage. Every X11 capture of `:0` sees black and
  zero damage events while every extension answers, which is what makes it
  look like a bug. The Linux backend is checked against Xvfb (`:99`,
  1280x720x24, on its socket, which needs `/tmp/.X11-unix` remounted
  writable in WSL; it reverts on `wsl --shutdown`) and, later, Xvfb on
  azure. Colours proved the byte order: steelblue 70/130/180 lands as BGRX
  [180, 130, 70].
- **2026-09-18, X11 reports no moves, ever.** A scroll is damage over the
  scrolled area, never a shifted block. With DXGI reporting none on Windows
  11 either, CopyRect on both platforms rests on phase 2's compare pass.
- **2026-09-18, fetching X damage: the bounding box beats the rectangles
  once they are many.** Release build, adjacent runs, a fixed scenario of
  160 banded rectangles on the 1280x720 Xvfb: server CPU 10 to 20 ms per
  3.1 s scenario with one GetImage of the bounding box, 140 to 160 ms with
  one per rectangle, pipelined. Each rectangle costs the X server a GetImage
  and this side a reply to parse, about twenty microseconds, while the whole
  screen is one or two milliseconds. The backend fetches one by one up to
  64 rectangles and the bounding box past that; comparing areas rather than
  counts is on the backlog, behind phase 2, which changes the shape of the
  damage anyway.
- **2026-09-18, the compare pass, measured.** Release build, adjacent runs.
  X11 on a rooted 1280x720 Xvfb with a clock ticking and eyes following the
  pointer, 141 frames: 7.47 M pixels reported, 2.18 M after, a ratio of
  0.29 (0.19 without the opening full frame); the clock's tick is reported
  as one 199x199 rectangle and goes out as 6,300 to 7,000 pixels, the hands
  and the marks; 155 µs a frame mean, 1.4 ms for the opening frame. At
  1080p, 134 µs a frame for ordinary movement and 3.6 ms for a whole-screen
  repaint, of which 2.55 ms is hashing 8.3 MB (about 3.3 GB/s here) and
  1.05 ms the scroll detector. DXGI on this laptop's desktop, busy, 24 s,
  1657 frames: 76.5 M pixels reported, 17.3 M after, a ratio of 0.23; 545
  of the frames, one in three, changed no pixel at all and no client hears
  of them now; 24 scrolls found; 0.30 ms a frame mean. Detection is per
  cell, not per row: a real scroll never owns whole rows (a scrollbar at
  least), so row matching finds no scroll that exists; every copy emitted
  is verified cell against cell, so a wrong guess can only copy pixels that
  were going to be sent anyway. Horizontal scrolls and two panes scrolling
  by different amounts are not detected; the backlog has both.
- **2026-09-18, the compare pass costs 39% on a backend that tells the
  truth.** The synthetic screen reports exact rectangles and its own moves;
  the pass rounds everything to 64-pixel cells, so its damage came out at
  1.39 times what was reported (15.1 M reported, 20.9 M after, over a 30 s
  run). The cell grid is the price of not keeping the previous frame: a
  bargain against a backend that reports a whole window, a loss against one
  that reports the truth, and the synthetic screen is the only one that
  does. Noticing an exact backend and stepping aside is on the backlog.
- **2026-09-18, flow control, measured.** Release, adjacent. In process
  (`cargo run --release -p alloyvnc --example pace`), the synthetic screen
  at 30 fps for 30 s: a client reading as fast as it can took 902 updates
  (30.0 a second), rtt 0.48 ms against a base of 0.33, window grown to
  3.7 MB, 83.5 MB in all, 901 of 902 updates under a millisecond from frame
  to socket. A client pausing 300 ms between reads, while the screen drew
  904 frames, took 97 updates and 51.3 MB: the 800 it could not read
  folded into the ones it did, the window settled at 455 KB against a
  300 ms round trip, and nothing in its histogram was over 100 ms; without
  the window those updates would have aged in a send buffer. The window
  opens 4 KiB an ack, so the fast client needed all 901 acks and the whole
  30 s to reach 3.7 MB; a slow-start phase is on the backlog.
- **2026-09-18, the browser on both screens, with the stats endpoint.**
  noVNC through websockify in a hidden rig tab, release. Synthetic screen
  at 30 fps, 20 s: 267 updates, one CopyRect in each (the band's scroll),
  37.8 MB, base rtt 2.3 ms but 55 ms smoothed, so the window stayed at
  64 KB and held the tab to 13 updates a second: a hidden tab answers
  slowly, and the window read that as the queue it is. The real desktop,
  busy, 24 s: 1481 frames captured of which 729 changed no pixel, 34.3 M
  pixels reported and 5.45 M after (0.16), 457 updates and 11.45 MB to the
  browser (phase 1's Raw run moved 190 MB in 22 s), rtt 2.6 ms against a
  base of 1.0, window 868 KB, and from frame to socket 319 updates under a
  millisecond, 57 between 10 and 20 ms (the 60 Hz ceiling's slot) and none
  over 20.
- **2026-09-18, the encoders, measured.** Release, 300 frames of the
  1280x720 synthetic screen, `cargo run --release -p alloyvnc --example
  encoders`. On the screen's exact damage (5.6 M pixels), in the
  framebuffer's own format: Raw 22.5 MB in 3.4 ms; Hextile 2.5% of Raw in
  0.26 ms a frame; Tight 1.4% in 0.46 ms; ZRLE 1.1% in 2.8 ms. On coarse
  damage, the whole-window rectangles the real backends report (49 times
  the pixels): Tight 0.7% of Raw at 22 ms a frame, ZRLE 0.5% at 21.5 ms,
  Hextile 1.1% at 15 ms. Four conclusions. ZRLE is the smallest and not
  worth it: a fifth fewer bytes than Tight for six times the CPU, so
  section 9's "never ZRLE by default" is a number now. The compare pass is
  worth more than the encoder: coarse damage costs Tight 25 times the bytes
  and 49 times the time of exact damage. The pixel format matters only to
  Raw (a copy at 3.4 ms in BGRX, 32 ms through the packer in RGBX); the
  other encoders' own work dominates. And on this screen Tight never took
  the JPEG it was offered at quality 6: every piece fit a palette, which
  is smaller and lossless, so JPEG is proved on a gradient in the tests
  (four times smaller, worst channel 24 off) and the synthetic screen
  needs a photographic scenario before the quality levels can be measured.
- **2026-09-18, two things measured and left out.** A 32-bit byte-order
  fast path for noVNC's RGBX: the generic packer and a byte shuffle both
  run at 2.45 GB/s on a 1080p frame against a plain copy's 5.1, so the
  packer already runs at the memory's pace and a shuffle has nothing to
  win (behind a dispatch it was half as fast again); the narrow formats
  are where conversion time goes, rgb565 at 0.83 GB/s.
  `examples/convert-bench.rs` keeps the four formulations rerunnable.
  And libjpeg-turbo: it cannot build here, cmake and nasm being absent,
  so the JPEG encoder is the pure-Rust one behind a single function
  taking RGB rows, a level and a subsampling; the swap is one file when
  a build has the tools.
- **2026-09-18, Tight to the browser on the real desktop.** noVNC in a
  hidden rig tab (it asks for Tight at quality 6, compression 2), release,
  24 s once connected, the desktop busy with two sessions' output: 988
  updates, 11,773 rectangles, 31 of them CopyRect, 4.90 MB in all, 5 KB an
  update against 25 KB for phase 2b's Raw run on the same desktop. Where
  the bytes went: palette 2.69 MB, full-colour deflate 1.45 MB, JPEG
  0.61 MB, fill 16 bytes. The hidden tab answered pings at 32 ms smoothed
  against a base of 1.2, so the window sat at 180 KB; frame to socket 373
  updates under a millisecond and 292 between 10 and 20 ms, the 60 Hz slot.
  The TightVNC Server on 5900 is the baseline to put beside this; it needs
  Kyle's viewer and his password, so the byte-counting proxy at
  `C:\Users\Kyle\.claude\chrome\bench\count-proxy.py` is his to run:
  `python count-proxy.py 5902 127.0.0.1:5900`, the viewer pointed at 5902,
  and the same again at 5901 with alloyvnc serving.
- **2026-09-18, the clipboard, four things found by running it.** On
  Windows the clipboard sequence number does not move until
  CloseClipboard, so reading it before the handle is closed records the
  old value and the server's own write looks like somebody else's; the
  round-trip test failed on exactly that, and the client that pasted would
  have had its text handed back a tenth of a second later. On X11 nothing
  leaves the connection until it is flushed: a ConvertSelection and the
  SelectionNotify sat in the output buffer and the live test caught it on
  the first run. A modern X server keeps at most four core keysyms per key
  (ChangeKeyboardMapping is translated into XKB), so a lent keycode filled
  to seven levels read back with three NoSymbols and would not clear; two
  levels, and the restore verified with xmodmap after the process exits.
  And a display has one clipboard while cargo runs tests in parallel, so
  the two live X11 tests fought each other and are one test now. Measured:
  GetClipboardSequenceNumber costs 1.14 µs a call here (100k iterations,
  release), eleven microseconds of every second at the 100 ms cadence; a
  test asserts under 10 µs so a poll that has become a round trip is
  noticed. Proved end to end against a client sharing no code with the
  repo: U+00E9 and U+2713 survived a UTF-16 clipboard, a deflated provide
  and the socket.
- **2026-09-18, the clipboard against noVNC, two more.** noVNC writes its
  provide's zlib stream with a full flush and never finishes it: no final
  block, no Adler-32. A reader that inflates to the end of the stream
  rejects every paste from noVNC; the one that works, TigerVNC's, inflates
  exactly the bytes the embedded lengths call for and never asks for the
  end. The first build here did the former and, worse, let the rejection
  end the session; a bad clipboard message is a dropped message. Desk to
  browser was proved on the wire meanwhile: noVNC's `_writeClipboard`
  received "probe ✓ two" with the check mark intact. Its panel stayed
  empty because noVNC hands received text to the browser's asynchronous
  clipboard API first and only falls back to the panel when that API is
  absent; a hidden tab cannot write the browser clipboard, so a check from
  the rig reads the text at noVNC's own function, not in the panel. With
  the reader fixed, both directions hold against noVNC in the rig: "desk ✓
  café" reached noVNC's function, "from noVNC ✓ 42" reached the desk's
  clipboard with U+2713 intact, and the session stayed up through both.
- **2026-09-18, a flush is not an acknowledgement.** The X11 input side
  lends a spare keycode to a keysym the layout cannot type and gives it
  back on the way out. Its Drop flushed the ChangeKeyboardMapping and
  closed the connection; a flush says the bytes left this process, not
  that the server acted on them, so the request could still be in flight
  when the socket went and the keycode stayed bound on the user's display
  for the rest of the X session. Intermittent, which is why earlier runs
  looked clean. The restore is a checked request now, which round-trips,
  and the live test clears any leftover before it starts. A process killed
  outright still leaks one keycode, since nothing outside it records the
  loan; on the backlog.
- **2026-09-18, the desk's clipboard is contended.** During the rig check,
  PowerShell's Set-Clipboard once reported "Requested Clipboard operation
  did not succeed" and then succeeded: two processes wanted the clipboard
  in the same instant, one of them this server reading the change. Any
  clipboard code on Windows has to retry OpenClipboard a few times, which
  ours does; other programs may not.
