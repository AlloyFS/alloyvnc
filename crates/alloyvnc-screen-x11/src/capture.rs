//! The X11 desktop: XDamage for what changed, MIT-SHM for the pixels.
//!
//! There is no compositor here handing over a finished frame, so the frame
//! is assembled. XDamage is asked to watch the root window and the server
//! then sends an event whenever anything is drawn into it; the rectangles
//! themselves are collected with DamageSubtract, which hands back everything
//! accumulated since the last call and empties the record in one step, so
//! nothing is counted twice and nothing is missed between two calls.
//!
//! The pixels come through MIT-SHM. A plain GetImage sends the whole
//! rectangle down the socket, which for a screenful is eight megabytes of
//! protocol per frame. A shared-memory segment is a block of memory that the
//! X server and this process both map at once, so GetImage becomes "write it
//! there" and the reply is four bytes.
//!
//! The segment is the server's own: ShmCreateSegment asks it to allocate and
//! hands back a file descriptor to mmap, rather than the older ShmAttach
//! where the client makes a System V segment and passes its numeric id. Both
//! exist; the older one is refused outright by some servers, because a
//! numeric id is something a client could have guessed at and the server has
//! to check who owns it. A descriptor needs no such check, so the fd route
//! is the one that works everywhere it is offered.
//!
//! What X11 will not say is that a block moved. A scroll arrives as damage
//! over the scrolled area like any other drawing, so [`Frame::moves`] is
//! always empty here and CopyRect has to come from the compare pass.

use std::os::fd::AsRawFd;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloyvnc_screen::{Capture, CaptureError, CursorShape, Frame, Framebuffer, Rect, Region};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use crate::cursor;

/// Above this many damage rectangles the bounding box is fetched in one
/// request instead of one per rectangle.
///
/// The per-rectangle path is pipelined, so N rectangles cost one round trip
/// rather than N, and the first guess was that it would always win. It does
/// not. Measured on a 1280x720 Xvfb whose damage arrives in 160 banded
/// rectangles (a pointer crossing the screen with two clients redrawing),
/// release build, the same scenario four times with the runs adjacent: the
/// server burned 10 and 20 ms of CPU taking the bounding box against 140 and
/// 160 ms fetching each rectangle. Sixteen small transfers are cheaper than
/// one big one; a hundred and sixty are not, because each one still costs
/// the server a GetImage of its own and this side a reply to parse, about
/// twenty microseconds a rectangle, while the whole screen in one piece is
/// one or two milliseconds.
///
/// Sixty-four is where those two meet for a bounding box the size of this
/// screen. The proper rule compares areas rather than counts, since a
/// hundred rectangles covering a twentieth of the screen are still worth
/// fetching one by one; that is on the backlog, behind the compare pass of
/// phase 2, which changes the shape of the damage anyway.
const MAX_FETCHES: usize = 64;

/// What the framebuffer needs the server to be: 24-bit colour in 32-bit
/// words, blue byte first in memory. Anything else would need a conversion
/// on every pixel, and there is nothing to convert from on a normal desktop.
const WANTED: (u32, u32, u32) = (0x00ff_0000, 0x0000_ff00, 0x0000_00ff);

fn failed(what: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Failed(format!("{what}: {e}"))
}

fn lost(what: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Lost(format!("{what}: {e}"))
}

/// A shared memory segment: allocated by the X server, mapped here, big
/// enough for one screenful of 32-bit pixels.
struct Shared {
    addr: *mut u8,
    len: usize,
    seg: shm::Seg,
}

impl Shared {
    fn new(conn: &RustConnection, len: usize) -> Result<Shared, CaptureError> {
        let seg = conn
            .generate_id()
            .map_err(|e| failed("no resource id for the segment", e))?;
        // Not read-only: the server writes every frame into it, and this
        // side only ever reads.
        let reply = conn
            .shm_create_segment(seg, len as u32, false)
            .map_err(|e| failed("ask the server for a segment", e))?
            .reply()
            .map_err(|e| failed("the server refused a segment", e))?;
        // SAFETY: the descriptor is the one the server just made for this
        // segment and `len` is the size it was asked for. MAP_SHARED is what
        // makes the server's writes visible here rather than a private copy.
        let addr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                reply.shm_fd.as_raw_fd(),
                0,
            )
        };
        // The descriptor has done its work: a mapping outlives the
        // descriptor it was made from, and dropping the reply closes it.
        drop(reply);
        if addr == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            let _ = conn.shm_detach(seg);
            return Err(failed("map the segment", e));
        }
        tracing::debug!(bytes = len, "shared memory segment mapped");
        Ok(Shared {
            addr: addr.cast(),
            len,
            seg,
        })
    }

    /// The segment as bytes. Only read after the reply to a ShmGetImage,
    /// which the server sends once it has finished writing.
    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes and lives as long as this
        // value; nothing hands out a mutable view of it.
        unsafe { std::slice::from_raw_parts(self.addr, self.len) }
    }

    fn release(&mut self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.seg);
        let _ = conn.flush();
        // SAFETY: unmapping the mapping this value owns, once.
        unsafe { libc::munmap(self.addr.cast(), self.len) };
    }
}

/// Everything tied to one connection. Replaced wholesale when the
/// connection breaks, which is what makes a lost X server recoverable.
struct Live {
    conn: RustConnection,
    root: xproto::Window,
    size: (u16, u16),
    damage: damage::Damage,
    /// The region DamageSubtract empties into, made once and reused.
    region: xfixes::Region,
    shm: Shared,
    /// Whether RandR is new enough to list monitors one by one.
    monitors: bool,
}

impl Live {
    fn open(display: Option<&str>) -> Result<Live, CaptureError> {
        let (conn, screen_num) = x11rb::connect(display).map_err(|e| failed("connect to the X server", e))?;
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or_else(|| CaptureError::Failed(format!("no screen {screen_num}")))?;
        let root = screen.root;
        let size = (screen.width_in_pixels, screen.height_in_pixels);
        check_format(setup, screen)?;

        // Every extension is asked for its version first: the query is what
        // loads the extension and tells us it is there at all.
        let damage_version = conn
            .damage_query_version(1, 1)
            .map_err(|e| failed("the server has no DAMAGE extension", e))?
            .reply()
            .map_err(|e| failed("DAMAGE version", e))?;
        let xfixes_version = conn
            .xfixes_query_version(5, 0)
            .map_err(|e| failed("the server has no XFIXES extension", e))?
            .reply()
            .map_err(|e| failed("XFIXES version", e))?;
        if xfixes_version.major_version < 4 {
            return Err(CaptureError::Failed(format!(
                "XFIXES {}.{} is too old to report the pointer; 4.0 is the minimum",
                xfixes_version.major_version, xfixes_version.minor_version
            )));
        }
        let shm_version = conn
            .shm_query_version()
            .map_err(|e| failed("the server has no MIT-SHM extension", e))?
            .reply()
            .map_err(|e| failed("MIT-SHM version", e))?;
        if (shm_version.major_version, shm_version.minor_version) < (1, 2) {
            return Err(CaptureError::Failed(format!(
                "MIT-SHM {}.{} cannot hand over a segment by descriptor; 1.2 is the minimum",
                shm_version.major_version, shm_version.minor_version
            )));
        }
        let randr_version = conn
            .randr_query_version(1, 5)
            .map_err(|e| failed("the server has no RANDR extension", e))?
            .reply()
            .map_err(|e| failed("RANDR version", e))?;
        let monitors = (randr_version.major_version, randr_version.minor_version) >= (1, 5);
        tracing::debug!(
            damage = format_args!(
                "{}.{}",
                damage_version.major_version, damage_version.minor_version
            ),
            xfixes = format_args!(
                "{}.{}",
                xfixes_version.major_version, xfixes_version.minor_version
            ),
            shm = format_args!("{}.{}", shm_version.major_version, shm_version.minor_version),
            randr = format_args!("{}.{}", randr_version.major_version, randr_version.minor_version),
            "extensions"
        );

        // NonEmpty means one event per run of drawing rather than one per
        // rectangle: the server stops reporting until the record is emptied
        // with DamageSubtract, so a busy screen costs one event, not
        // thousands.
        let damage = conn
            .generate_id()
            .map_err(|e| failed("no resource id for damage", e))?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)
            .map_err(|e| failed("watch the root window", e))?
            .check()
            .map_err(|e| failed("watch the root window", e))?;
        let region = conn
            .generate_id()
            .map_err(|e| failed("no resource id for the region", e))?;
        conn.xfixes_create_region(region, &[])
            .map_err(|e| failed("create the damage region", e))?
            .check()
            .map_err(|e| failed("create the damage region", e))?;
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)
            .map_err(|e| failed("watch the pointer", e))?
            .check()
            .map_err(|e| failed("watch the pointer", e))?;
        conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)
            .map_err(|e| failed("watch for a resolution change", e))?
            .check()
            .map_err(|e| failed("watch for a resolution change", e))?;

        let shm = Shared::new(&conn, bytes_for(size))?;
        tracing::info!(width = size.0, height = size.1, "capturing the X display");
        Ok(Live {
            conn,
            root,
            size,
            damage,
            region,
            shm,
            monitors,
        })
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Best effort: a broken connection cannot be told anything, and the
        // server frees a client's resources when it goes away regardless.
        let _ = self.conn.damage_destroy(self.damage);
        let _ = self.conn.xfixes_destroy_region(self.region);
        self.shm.release(&self.conn);
    }
}

fn bytes_for(size: (u16, u16)) -> usize {
    size.0 as usize * size.1 as usize * Framebuffer::BYTES_PER_PIXEL
}

/// Refuse anything the framebuffer would have to convert.
///
/// The framebuffer holds 32-bit words with the blue byte first, which is
/// what a normal 24-bit TrueColor visual on a little-endian machine puts in
/// memory: the red mask is 0xff0000, so red is the third byte of the word,
/// and a little-endian word writes its low byte first. Refusing the rest is
/// honest; a 16-bit or big-endian server needs a converting blit that
/// nothing on a modern desktop would ever exercise.
fn check_format(setup: &xproto::Setup, screen: &xproto::Screen) -> Result<(), CaptureError> {
    let visual = screen
        .allowed_depths
        .iter()
        .find(|d| d.depth == screen.root_depth)
        .and_then(|d| d.visuals.iter().find(|v| v.visual_id == screen.root_visual))
        .ok_or_else(|| CaptureError::Failed("the root visual is not in the screen".into()))?;
    let bits = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .map(|f| f.bits_per_pixel)
        .unwrap_or(0);
    let masks = (visual.red_mask, visual.green_mask, visual.blue_mask);
    if screen.root_depth != 24
        || bits != 32
        || masks != WANTED
        || setup.image_byte_order != ImageOrder::LSB_FIRST
    {
        return Err(CaptureError::Failed(format!(
            "this display is depth {} in {} bits, masks {:#x}/{:#x}/{:#x}, {:?}; \
             the framebuffer needs depth 24 in 32 bits, {:#x}/{:#x}/{:#x}, LSB first",
            screen.root_depth,
            bits,
            masks.0,
            masks.1,
            masks.2,
            setup.image_byte_order,
            WANTED.0,
            WANTED.1,
            WANTED.2
        )));
    }
    Ok(())
}

pub struct X11Capture {
    live: Live,
    /// The display this was opened on, so a broken connection can be
    /// reopened on the same one.
    display: Option<String>,
    /// Reopen the connection before the next wait.
    reopen: bool,
    /// The next frame is the whole screen: nothing of it has reached the
    /// framebuffer yet, or the screen changed size under it.
    refresh: bool,
    /// Something was drawn; the rectangles are collected once the event
    /// queue is drained, so a burst of drawing costs one DamageSubtract.
    drawn: bool,
    /// The pointer changed; its image is fetched once per wait.
    pointer_changed: bool,
    /// The screen changed size.
    resized: bool,
    /// Where each fetched rectangle sits in the shared segment, waiting for
    /// [`Capture::apply`] to copy it out.
    fetched: Vec<(Rect, usize)>,
    pending_cursor: Option<Arc<CursorShape>>,
}

// SAFETY: the raw pointer in the shared segment is the only thing here that
// is not Send by itself, and it points at a mapping owned by this value for
// its whole life. The structure moves to the capture thread once and is
// touched from nowhere else, so the mapping has one user at a time.
unsafe impl Send for X11Capture {}

impl X11Capture {
    /// Open `display`, or `$DISPLAY` when it is `None`.
    pub fn new(display: Option<&str>) -> Result<X11Capture, CaptureError> {
        Ok(X11Capture {
            live: Live::open(display)?,
            display: display.map(str::to_owned),
            reopen: false,
            refresh: true,
            drawn: false,
            pointer_changed: true,
            resized: false,
            fetched: Vec::new(),
            pending_cursor: None,
        })
    }

    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.live.size.0 as i32, self.live.size.1 as i32)
    }

    /// A frame is sitting here waiting for [`Capture::apply`] to take it.
    fn ready(&self) -> bool {
        !self.fetched.is_empty() || self.pending_cursor.is_some() || self.resized
    }

    /// There is something to collect once the event queue is drained. The
    /// first wait of all has `refresh` set, so the whole screen goes out
    /// without waiting for anything to be drawn into it.
    fn outstanding(&self) -> bool {
        self.drawn || self.refresh || self.pointer_changed || self.resized
    }

    /// Block until the connection has something to read or `timeout` passes.
    ///
    /// x11rb has no timed wait, and its blocking one would hold the capture
    /// thread past the stop flag. The connection's socket is a file
    /// descriptor like any other, so one poll(2) on it does the waiting: one
    /// syscall per wait, no thread and no polling loop. Anything already
    /// buffered inside x11rb has to be drained first, or a wait would block
    /// on a socket whose news has already been read.
    fn wait_readable(&self, timeout: Duration) -> Result<bool, CaptureError> {
        let fd = self.live.conn.stream().as_raw_fd();
        let mut poller = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one descriptor structure, owned by this frame, for the
        // length of the call.
        let ready = unsafe { libc::poll(&mut poller, 1, ms) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            // A signal interrupting the wait is not a failure; the caller
            // comes straight back round.
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(failed("poll the X connection", e));
        }
        Ok(ready > 0)
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::DamageNotify(_) => self.drawn = true,
            Event::XfixesCursorNotify(_) => self.pointer_changed = true,
            Event::RandrScreenChangeNotify(_) => self.resized = true,
            Event::Error(e) => tracing::warn!(error = ?e, "the X server reported an error"),
            _ => {}
        }
    }

    /// Re-read the root's geometry after a resize and make a segment to fit.
    fn regeometry(&mut self) -> Result<(), CaptureError> {
        let geometry = self
            .live
            .conn
            .get_geometry(self.live.root)
            .map_err(|e| lost("ask for the root geometry", e))?
            .reply()
            .map_err(|e| lost("the root geometry", e))?;
        let size = (geometry.width, geometry.height);
        if size == self.live.size {
            return Ok(());
        }
        tracing::info!(
            from = format_args!("{}x{}", self.live.size.0, self.live.size.1),
            to = format_args!("{}x{}", size.0, size.1),
            "the display changed size"
        );
        let shm = Shared::new(&self.live.conn, bytes_for(size))?;
        let mut old = std::mem::replace(&mut self.live.shm, shm);
        old.release(&self.live.conn);
        self.live.size = size;
        self.refresh = true;
        Ok(())
    }

    /// Take everything drawn since the last call, as rectangles in picture
    /// coordinates.
    fn take_damage(&mut self) -> Result<Vec<Rect>, CaptureError> {
        // Subtract empties the damage record into the region in one step,
        // so drawing that lands between the two is kept for the next frame
        // rather than lost.
        self.live
            .conn
            .damage_subtract(self.live.damage, x11rb::NONE, self.live.region)
            .map_err(|e| lost("subtract the damage", e))?;
        let region = self
            .live
            .conn
            .xfixes_fetch_region(self.live.region)
            .map_err(|e| lost("fetch the damage region", e))?
            .reply()
            .map_err(|e| lost("the damage region", e))?;
        let bounds = self.bounds();
        Ok(region
            .rectangles
            .iter()
            .map(|r| Rect::new(r.x as i32, r.y as i32, r.width as i32, r.height as i32).intersection(&bounds))
            .filter(|r| !r.is_empty())
            .collect())
    }

    /// Ask the server to write each rectangle into the shared segment.
    ///
    /// Every request goes out before any reply is read, so the whole set
    /// costs one round trip however many rectangles there are. That is what
    /// makes fetching them one by one worth doing rather than always taking
    /// the bounding box: only the pixels that changed cross the boundary.
    fn fetch(&mut self, mut rects: Vec<Rect>) -> Result<(), CaptureError> {
        if rects.is_empty() {
            return Ok(());
        }
        if rects.len() > MAX_FETCHES {
            let box_ = rects.iter().fold(Rect::EMPTY, |acc, r| acc.union_bounds(r));
            tracing::trace!(
                rects = rects.len(),
                "too many rectangles; taking the bounding box"
            );
            rects = vec![box_];
        }
        let format = u8::from(ImageFormat::Z_PIXMAP);
        let mut offset = 0usize;
        let mut cookies = Vec::with_capacity(rects.len());
        for rect in &rects {
            let bytes = rect.area() as usize * Framebuffer::BYTES_PER_PIXEL;
            if offset + bytes > self.live.shm.len {
                // The rectangles are a region, so they do not overlap and
                // cannot outgrow a screenful. Belt and braces.
                tracing::warn!(
                    offset,
                    bytes,
                    "the damage does not fit the segment; dropping the rest"
                );
                break;
            }
            let cookie = self
                .live
                .conn
                .shm_get_image(
                    self.live.root,
                    rect.x1 as i16,
                    rect.y1 as i16,
                    rect.width() as u16,
                    rect.height() as u16,
                    !0,
                    format,
                    self.live.shm.seg,
                    offset as u32,
                )
                .map_err(|e| lost("ask for the pixels", e))?;
            cookies.push((*rect, offset, cookie));
            offset += bytes;
        }
        self.live
            .conn
            .flush()
            .map_err(|e| lost("flush the image requests", e))?;
        let mut done = Vec::with_capacity(cookies.len());
        for (rect, offset, cookie) in cookies {
            cookie.reply().map_err(|e| lost("read the pixels", e))?;
            done.push((rect, offset));
        }
        self.fetched.extend(done);
        Ok(())
    }

    fn read_cursor(&mut self) -> Result<(), CaptureError> {
        let image = self
            .live
            .conn
            .xfixes_get_cursor_image()
            .map_err(|e| lost("ask for the pointer", e))?
            .reply()
            .map_err(|e| lost("the pointer image", e))?;
        let shape = cursor::convert(
            image.width,
            image.height,
            (image.xhot, image.yhot),
            &image.cursor_image,
        );
        self.pending_cursor = Some(Arc::new(shape));
        Ok(())
    }
}

impl Capture for X11Capture {
    fn size(&self) -> (u32, u32) {
        (self.live.size.0 as u32, self.live.size.1 as u32)
    }

    fn screens(&self) -> Vec<Rect> {
        let whole = || {
            let (w, h) = self.size();
            vec![Rect::new(0, 0, w as i32, h as i32)]
        };
        if !self.live.monitors {
            return whole();
        }
        let reply = self
            .live
            .conn
            .randr_get_monitors(self.live.root, true)
            .map_err(|e| e.to_string())
            .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()));
        let monitors = match reply {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!(error = %e, "RandR would not list the monitors");
                return whole();
            }
        };
        let rects: Vec<Rect> = monitors
            .monitors
            .iter()
            .map(|m| Rect::new(m.x as i32, m.y as i32, m.width as i32, m.height as i32))
            .filter(|r| !r.is_empty())
            .collect();
        if rects.is_empty() { whole() } else { rects }
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, CaptureError> {
        if self.reopen {
            // A display that has gone away and not come back yet is lost,
            // not failed: the difference is whether the caller should keep
            // trying, and here it should. The first open of all reports
            // Failed, because a display that is not there at startup is not
            // going to appear.
            self.live = Live::open(self.display.as_deref()).map_err(|e| CaptureError::Lost(e.to_string()))?;
            self.reopen = false;
            self.refresh = true;
            self.pointer_changed = true;
        }
        if self.ready() {
            return Ok(true);
        }
        let deadline = Instant::now() + timeout;
        loop {
            loop {
                match self.live.conn.poll_for_event() {
                    Ok(Some(event)) => self.handle(event),
                    Ok(None) => break,
                    Err(e) => {
                        self.reopen = true;
                        return Err(lost("read from the X server", e));
                    }
                }
            }
            // Events that say nothing about the picture (a keymap change,
            // another extension's news) do not end the wait on their own.
            if self.outstanding() {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.wait_readable(left)? {
                return Ok(false);
            }
        }

        if self.resized {
            self.regeometry()?;
        }
        // The damage record is emptied whether or not its rectangles are
        // used, so a full refresh does not leave a frame's worth behind for
        // the next one to report twice.
        let mut rects = if self.drawn {
            self.drawn = false;
            self.take_damage()?
        } else {
            Vec::new()
        };
        if self.refresh {
            self.refresh = false;
            rects = vec![self.bounds()];
        }
        self.fetch(rects)?;
        if self.pointer_changed {
            self.pointer_changed = false;
            self.read_cursor()?;
        }
        Ok(self.ready())
    }

    fn apply(&mut self, fb: &mut Framebuffer) -> Result<Frame, CaptureError> {
        let mut frame = Frame::default();
        let (width, height) = self.size();
        if fb.width() != width || fb.height() != height {
            fb.resize(width, height);
            frame.resized = true;
        }
        self.resized = false;
        frame.cursor = self.pending_cursor.take();

        let pixel = Framebuffer::BYTES_PER_PIXEL;
        // Taken out first: reading the segment borrows the connection's
        // half of this value, and draining the list borrows the other.
        let mut fetched = std::mem::take(&mut self.fetched);
        let mut damage = Vec::with_capacity(fetched.len());
        {
            let data = self.live.shm.bytes();
            for (rect, offset) in fetched.drain(..) {
                let width = rect.width() as u32;
                let row_bytes = width as usize * pixel;
                for (i, y) in (rect.y1..rect.y2).enumerate() {
                    let from = offset + i * row_bytes;
                    let row = fb.row_span_mut(y as u32, rect.x1 as u32, width);
                    row.copy_from_slice(&data[from..from + row_bytes]);
                }
                damage.push(rect);
            }
        }
        // The buffer goes back so the next frame does not allocate.
        self.fetched = fetched;
        if !damage.is_empty() {
            tracing::trace!(
                rects = damage.len(),
                pointer = frame.cursor.is_some(),
                "frame applied"
            );
            frame.damage = Region::from_rects(damage);
        }
        Ok(frame)
    }
}
