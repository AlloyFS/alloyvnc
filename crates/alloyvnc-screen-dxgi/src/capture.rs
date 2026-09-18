//! Desktop duplication.
//!
//! One duplication per monitor attached to the desktop, all tiled into one
//! picture whose origin is the top-left of the virtual desktop. Waiting is
//! DXGI's own AcquireNextFrame, which blocks inside the compositor until
//! something changes, so an idle desktop costs no CPU at all. Only the
//! rectangles a frame reports as changed are copied off the GPU: one
//! CopySubresourceRegion per rectangle into a CPU-readable staging texture,
//! one Map, the rows read out, one Unmap.
//!
//! Two vocabulary notes for the Win32 side of this. An HRESULT is how every
//! COM call reports itself: a 32-bit code whose top bit means failure, which
//! the `windows` crate turns into a `Result`. An interface like
//! `IDXGIOutputDuplication` is a pointer to an object with a reference
//! count; cloning one adds a reference and dropping one removes it, so
//! ownership here is the same idea as an `Arc`, managed by the crate.

use std::mem::size_of;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use alloyvnc_screen::{Capture, CaptureError, CursorShape, Frame, Framebuffer, Rect, Region};
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::{Error as WinError, HRESULT, Interface};

use crate::{cursor, geom};

/// The longest one AcquireNextFrame blocks when several monitors are polled
/// in turn, so a quiet monitor cannot hold up a busy one for a whole wait.
const SLICE: Duration = Duration::from_millis(16);

/// Whether an HRESULT means this duplication is over rather than broken.
/// A mode change, a UAC prompt, the lock screen and a disconnected session
/// all end it; the answer is to build a new one, not to give up.
fn is_lost(code: HRESULT) -> bool {
    code == DXGI_ERROR_ACCESS_LOST || code == E_ACCESSDENIED || code == DXGI_ERROR_SESSION_DISCONNECTED
}

fn classify(e: &WinError, what: &str) -> CaptureError {
    let message = format!("{what}: {e}");
    if is_lost(e.code()) {
        CaptureError::Lost(message)
    } else {
        CaptureError::Failed(message)
    }
}

fn millis(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

fn from_rect(r: RECT) -> Rect {
    Rect::from_corners(r.left, r.top, r.right, r.bottom)
}

/// Buffers kept between frames, so capture at 60 Hz allocates nothing.
#[derive(Default)]
struct Scratch {
    /// DXGI's metadata block. Words rather than bytes because what it holds
    /// is rectangles of 32-bit integers, and reading those out of a buffer
    /// requires the buffer to start on a four-byte boundary; a `Vec<u8>` is
    /// only promised to start on a one-byte one.
    meta: Vec<u32>,
    /// The rectangles to copy, in one monitor's own coordinates.
    rects: Vec<Rect>,
    /// What changed, in picture coordinates, across every monitor.
    damage: Vec<Rect>,
}

/// A frame acquired and not yet released.
struct Held {
    info: DXGI_OUTDUPL_FRAME_INFO,
    /// The desktop as the compositor last presented it. Released with the
    /// frame, so nothing may outlive the matching ReleaseFrame.
    resource: IDXGIResource,
}

/// The pointer, which is one thing across every monitor even though each
/// duplication reports it separately.
struct Pointer {
    /// The shape bytes as DXGI hands them over, reused frame to frame.
    bytes: Vec<u8>,
    /// The last shape seen, whether or not it is on screen now.
    shape: Option<Arc<CursorShape>>,
    hidden: Arc<CursorShape>,
    /// Whether the pointer is on screen, as the newest report says.
    visible: bool,
    /// Whether the client has been sent a shape rather than a hidden one.
    shown: bool,
    /// The timestamp of the newest pointer report believed. With two
    /// monitors the one the pointer is not on reports it invisible on every
    /// frame, and believing that would flicker it away; the older report
    /// loses.
    at: i64,
}

impl Pointer {
    fn new() -> Pointer {
        Pointer {
            bytes: Vec::new(),
            shape: None,
            hidden: Arc::new(CursorShape::hidden()),
            visible: false,
            shown: false,
            at: 0,
        }
    }

    /// Fold one frame's pointer news into `frame`. Returns whether anything
    /// about the pointer changed.
    fn update(
        &mut self,
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        frame: &mut Frame,
    ) -> Result<bool, CaptureError> {
        let fresh_shape = info.PointerShapeBufferSize > 0;
        if fresh_shape {
            let size = info.PointerShapeBufferSize as usize;
            if self.bytes.len() < size {
                self.bytes.resize(size, 0);
            }
            let mut needed = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: the buffer is at least `size` bytes, which is what
            // DXGI said the shape needs, and the two out parameters point
            // at locals that outlive the call.
            unsafe {
                dup.GetFramePointerShape(
                    size as u32,
                    self.bytes.as_mut_ptr().cast(),
                    &mut needed,
                    &mut shape,
                )
            }
            .map_err(|e| classify(&e, "read the pointer shape"))?;
            let hot = (shape.HotSpot.x.max(0) as u32, shape.HotSpot.y.max(0) as u32);
            let converted = cursor::convert(
                shape.Type,
                shape.Width,
                shape.Height,
                shape.Pitch,
                hot,
                &self.bytes[..size],
            );
            self.shape = Some(Arc::new(converted));
        }
        if info.LastMouseUpdateTime != 0 && info.LastMouseUpdateTime >= self.at {
            self.at = info.LastMouseUpdateTime;
            self.visible = info.PointerPosition.Visible.as_bool();
        }
        if self.visible {
            if let Some(shape) = &self.shape
                && (fresh_shape || !self.shown)
            {
                frame.cursor = Some(shape.clone());
                self.shown = true;
            }
        } else if self.shown {
            frame.cursor = Some(self.hidden.clone());
            self.shown = false;
        }
        Ok(frame.cursor.is_some())
    }
}

/// One monitor: the duplication that feeds it, the device context that
/// copies for it, and the staging texture it copies into.
struct Output {
    /// Its place in the enumeration, for the log.
    index: usize,
    dup: IDXGIOutputDuplication,
    context: ID3D11DeviceContext,
    /// A texture the CPU can read. Video memory is not addressable from
    /// here at all, so the dirty rectangles are copied by the GPU into this
    /// one, which lives where both sides can reach it.
    staging: ID3D11Texture2D,
    /// The duplication's texture size, which is the monitor's own pixels.
    tex: (u32, u32),
    /// Where the monitor's top-left sits in the picture.
    off: (i32, i32),
    /// The monitor in picture coordinates, as a client is told about it.
    rect: Rect,
    /// Where the monitor sits on the desktop, kept while the union is
    /// worked out.
    desktop: RECT,
    held: Option<Held>,
    /// Nothing of this monitor has reached the framebuffer yet, so the next
    /// frame copies all of it rather than the rectangles DXGI reports.
    first: bool,
}

impl Output {
    /// The part of this monitor that lands inside the picture, in the
    /// monitor's own coordinates. Everything is clipped to it, so no copy
    /// can run off either end.
    fn reach(&self, fb: &Framebuffer) -> Rect {
        Rect::new(0, 0, self.tex.0 as i32, self.tex.1 as i32)
            .translate(self.off.0, self.off.1)
            .intersection(&fb.bounds())
            .translate(-self.off.0, -self.off.1)
    }

    /// Write the frame this monitor holds into `fb` and add what it changed
    /// to `frame`. The frame is released either way.
    fn apply(
        &mut self,
        fb: &mut Framebuffer,
        scratch: &mut Scratch,
        pointer: &mut Pointer,
        frame: &mut Frame,
    ) -> Result<(), CaptureError> {
        let Some(held) = self.held.take() else {
            return Ok(());
        };
        let applied = self.write(&held, fb, scratch, pointer, frame);
        // The desktop image goes before the frame does: DXGI takes the
        // frame back only once nothing else holds a reference to it.
        drop(held);
        // SAFETY: one ReleaseFrame for the one AcquireNextFrame that put
        // this frame in `held`.
        let released = unsafe { self.dup.ReleaseFrame() };
        applied.and_then(|()| released.map_err(|e| classify(&e, "release the frame")))
    }

    fn write(
        &mut self,
        held: &Held,
        fb: &mut Framebuffer,
        scratch: &mut Scratch,
        pointer: &mut Pointer,
        frame: &mut Frame,
    ) -> Result<(), CaptureError> {
        let info = &held.info;
        // The pointer first: a frame can carry a new shape and no pixels.
        let pointer_changed = pointer.update(&self.dup, info, frame)?;
        if info.LastPresentTime == 0 {
            // Nothing was presented. Either the pointer moved and nothing
            // else, or the frame is empty.
            if pointer_changed {
                tracing::trace!(output = self.index, "pointer only");
            }
            return Ok(());
        }
        let texture: ID3D11Texture2D = held
            .resource
            .cast()
            .map_err(|e| classify(&e, "the frame is not a 2D texture"))?;

        let Scratch { meta, rects, damage } = scratch;
        rects.clear();
        let reach = self.reach(fb);
        let mut moved = 0usize;
        if self.first {
            // The whole monitor, because the framebuffer holds nothing of
            // it yet. Its moves are meaningless against a blank picture.
            rects.push(reach);
            self.first = false;
        } else if info.TotalMetadataBufferSize > 0 {
            moved = self.metadata(info, meta, rects, reach, frame)?;
        }
        damage.extend(rects.iter().map(|r| r.translate(self.off.0, self.off.1)));
        self.copy(&texture, rects, fb)?;
        tracing::trace!(
            output = self.index,
            dirty = rects.len(),
            moves = moved,
            pointer = pointer_changed,
            "frame applied"
        );
        Ok(())
    }

    /// Read the frame's move and dirty rectangles. Both are in the
    /// monitor's own coordinates. Returns how many moves there were.
    fn metadata(
        &self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        meta: &mut Vec<u32>,
        rects: &mut Vec<Rect>,
        reach: Rect,
        frame: &mut Frame,
    ) -> Result<usize, CaptureError> {
        let total = info.TotalMetadataBufferSize as usize;
        let words = total.div_ceil(size_of::<u32>());
        if meta.len() < words {
            meta.resize(words, 0);
        }

        let mut used = 0u32;
        // SAFETY: the buffer holds at least `total` bytes and begins on a
        // four-byte boundary, which is the alignment a rectangle of LONGs
        // needs; DXGI reports through `used` how many bytes it wrote.
        unsafe {
            self.dup
                .GetFrameMoveRects(total as u32, meta.as_mut_ptr().cast(), &mut used)
        }
        .map_err(|e| classify(&e, "read the move rectangles"))?;
        let count = used as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        {
            // SAFETY: DXGI wrote `count` whole structures at the head of
            // the buffer, which outlives this borrow.
            let moves =
                unsafe { std::slice::from_raw_parts(meta.as_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(), count) };
            for m in moves {
                let src = (m.SourcePoint.x, m.SourcePoint.y);
                let Some(moved) = geom::clip_move(src, from_rect(m.DestinationRect), reach, self.off) else {
                    continue;
                };
                // The destination is dirty as well, and its pixels are
                // copied like any other rectangle. The picture is complete
                // whether or not the session turns the move into a
                // CopyRect, which it only may when the client's view of the
                // source is current.
                rects.push(moved.dst.translate(-self.off.0, -self.off.1));
                frame.moves.push(moved);
            }
        }

        // The moves are copies now, so the dirty rectangles may have the
        // whole buffer rather than what is left of it.
        let mut used = 0u32;
        // SAFETY: as above, and a RECT has the same alignment.
        unsafe {
            self.dup
                .GetFrameDirtyRects(total as u32, meta.as_mut_ptr().cast(), &mut used)
        }
        .map_err(|e| classify(&e, "read the dirty rectangles"))?;
        let dirty_count = used as usize / size_of::<RECT>();
        // SAFETY: DXGI wrote `dirty_count` whole rectangles at the head of
        // the buffer, which outlives this borrow.
        let dirty = unsafe { std::slice::from_raw_parts(meta.as_ptr().cast::<RECT>(), dirty_count) };
        for r in dirty {
            let rect = from_rect(*r).intersection(&reach);
            if !rect.is_empty() {
                rects.push(rect);
            }
        }
        Ok(count)
    }

    /// Copy `rects` out of the frame and into the framebuffer. The
    /// rectangles are in the monitor's own coordinates and already clipped.
    fn copy(
        &self,
        texture: &ID3D11Texture2D,
        rects: &[Rect],
        fb: &mut Framebuffer,
    ) -> Result<(), CaptureError> {
        if rects.is_empty() {
            return Ok(());
        }
        for r in rects {
            let region = D3D11_BOX {
                left: r.x1 as u32,
                top: r.y1 as u32,
                front: 0,
                right: r.x2 as u32,
                bottom: r.y2 as u32,
                back: 1,
            };
            // SAFETY: the box is inside the source texture and the staging
            // texture is the same size, so the destination corner is inside
            // it too. The call only queues work on the GPU; nothing is read
            // until the Map below.
            unsafe {
                self.context.CopySubresourceRegion(
                    &self.staging,
                    0,
                    r.x1 as u32,
                    r.y1 as u32,
                    0,
                    texture,
                    0,
                    Some(&region),
                )
            };
        }

        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture was made with CPU read access and is
        // not mapped. Map blocks until the copies above have run: it is the
        // CPU's window onto memory the GPU was writing, and the driver will
        // not hand it over mid-write.
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
        }
        .map_err(|e| classify(&e, "map the staging texture"))?;
        let pitch = map.RowPitch as usize;
        // SAFETY: Map handed back a buffer of `pitch` bytes per row for the
        // texture's full height, valid until Unmap. Nothing returns between
        // here and that Unmap, and every row read below is inside a
        // rectangle already clipped to the texture.
        let src = unsafe { std::slice::from_raw_parts(map.pData as *const u8, pitch * self.tex.1 as usize) };
        for r in rects {
            let width = r.width() as u32;
            let bytes = width as usize * Framebuffer::BYTES_PER_PIXEL;
            for y in r.y1..r.y2 {
                let from = y as usize * pitch + r.x1 as usize * Framebuffer::BYTES_PER_PIXEL;
                let row = fb.row_span_mut((y + self.off.1) as u32, (r.x1 + self.off.0) as u32, width);
                row.copy_from_slice(&src[from..from + bytes]);
            }
        }
        // SAFETY: one Unmap for the one Map above, on the same subresource.
        unsafe { self.context.Unmap(&self.staging, 0) };
        Ok(())
    }
}

pub struct DxgiCapture {
    outputs: Vec<Output>,
    /// The virtual desktop in desktop coordinates. The picture's origin is
    /// its top-left corner, which is not (0, 0) when a monitor sits above
    /// or to the left of the primary one.
    bounds: Rect,
    /// Build new duplications before the next wait.
    rebuild: bool,
    /// The next applied frame reports the whole picture as damage, because
    /// the duplications behind it are new.
    refresh: bool,
    scratch: Scratch,
    pointer: Pointer,
}

// SAFETY: every handle in here is a COM interface pointer, which refers to
// an object owned by the process rather than by a thread. A D3D11 device
// made without D3D11_CREATE_DEVICE_SINGLETHREADED is free-threaded, and
// this structure is moved to the capture thread once and touched from
// nowhere else, so no call is ever made on it from two threads at a time.
unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    pub fn new() -> Result<DxgiCapture, CaptureError> {
        let mut capture = DxgiCapture {
            outputs: Vec::new(),
            bounds: Rect::EMPTY,
            rebuild: false,
            refresh: true,
            scratch: Scratch::default(),
            pointer: Pointer::new(),
        };
        capture.build()?;
        Ok(capture)
    }

    /// The picture's top-left corner in desktop coordinates: the origin the
    /// input side needs to turn a picture coordinate back into a desktop one.
    pub fn origin(&self) -> (i32, i32) {
        (self.bounds.x1, self.bounds.y1)
    }

    /// Enumerate the desktop from scratch: every adapter, every monitor
    /// attached to it, one device per adapter and one duplication per
    /// monitor. Called again whenever a duplication is lost, which is how a
    /// mode change or an unlock is recovered from.
    fn build(&mut self) -> Result<(), CaptureError> {
        // Dropping the old outputs releases the duplications, and with them
        // any frame still held.
        self.outputs.clear();

        // SAFETY: the factory is asked for by the IID of the interface the
        // binding names, and comes back with a reference the value owns.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| CaptureError::Failed(format!("no DXGI factory: {e}")))?;

        let mut outputs = Vec::new();
        let mut bounds = Rect::EMPTY;
        for a in 0u32.. {
            // SAFETY: an index walk that ends with DXGI_ERROR_NOT_FOUND.
            let adapter = match unsafe { factory.EnumAdapters1(a) } {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
            // One device per adapter, made only once the adapter turns out
            // to have a monitor worth duplicating.
            let mut device: Option<(ID3D11Device, ID3D11DeviceContext)> = None;
            for o in 0u32.. {
                // SAFETY: the same index walk, over the adapter's outputs.
                let output = match unsafe { adapter.EnumOutputs(o) } {
                    Ok(output) => output,
                    Err(_) => break,
                };
                // SAFETY: GetDesc fills a local it is handed by the binding.
                let desc =
                    unsafe { output.GetDesc() }.map_err(|e| classify(&e, "read the monitor description"))?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                    && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                {
                    static ROTATED: Once = Once::new();
                    ROTATED.call_once(|| {
                        tracing::warn!(
                            rotation = desc.Rotation.0,
                            "a rotated monitor is captured unrotated; its picture will be wrong"
                        );
                    });
                }

                let (d3d, context) = match &device {
                    Some(pair) => pair.clone(),
                    None => {
                        let pair = create_device(&adapter)?;
                        device = Some(pair.clone());
                        pair
                    }
                };
                let output1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| classify(&e, "this Windows has no IDXGIOutput1"))?;
                // SAFETY: the device outlives the duplication, which takes
                // its own reference on it.
                let dup = unsafe { output1.DuplicateOutput(&d3d) }
                    .map_err(|e| classify(&e, "duplicate the monitor"))?;
                // SAFETY: GetDesc on a duplication returns by value and
                // cannot fail.
                let dup_desc = unsafe { dup.GetDesc() };
                let tex = (dup_desc.ModeDesc.Width, dup_desc.ModeDesc.Height);
                let staging = staging_texture(&d3d, tex)?;

                bounds = bounds.union_bounds(&from_rect(desc.DesktopCoordinates));
                outputs.push(Output {
                    index: outputs.len(),
                    dup,
                    context,
                    staging,
                    tex,
                    off: (0, 0),
                    rect: Rect::EMPTY,
                    desktop: desc.DesktopCoordinates,
                    held: None,
                    first: true,
                });
            }
        }

        if outputs.is_empty() {
            return Err(CaptureError::Lost("no monitor is attached to the desktop".into()));
        }
        // The offsets need the union, so they are filled once it is known.
        for out in &mut outputs {
            out.off = (out.desktop.left - bounds.x1, out.desktop.top - bounds.y1);
            out.rect = from_rect(out.desktop).translate(-bounds.x1, -bounds.y1);
        }
        tracing::info!(
            monitors = outputs.len(),
            width = bounds.width(),
            height = bounds.height(),
            "duplicating the desktop"
        );
        self.outputs = outputs;
        self.bounds = bounds;
        self.refresh = true;
        Ok(())
    }

    /// One AcquireNextFrame on one monitor. `Ok(true)` means it is now
    /// holding a frame for [`Capture::apply`].
    fn acquire(&mut self, index: usize, ms: u32) -> Result<bool, CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: both out parameters point at locals that outlive the
        // call, and this monitor holds no frame, which is what would make a
        // second acquire an invalid call.
        let acquired = unsafe {
            self.outputs[index]
                .dup
                .AcquireNextFrame(ms, &mut info, &mut resource)
        };
        match acquired {
            Ok(()) => match resource {
                Some(resource) => {
                    self.outputs[index].held = Some(Held { info, resource });
                    Ok(true)
                }
                None => {
                    // Nothing to copy from, but the frame is still ours to
                    // give back.
                    // SAFETY: one release for the acquire above.
                    let _ = unsafe { self.outputs[index].dup.ReleaseFrame() };
                    Ok(false)
                }
            },
            // A timeout is the normal answer on a desktop where nothing is
            // happening, not a failure.
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(false),
            Err(e) => {
                let error = classify(&e, "acquire a frame");
                if matches!(error, CaptureError::Lost(_)) {
                    self.outputs.clear();
                    self.rebuild = true;
                }
                Err(error)
            }
        }
    }
}

fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
    let mut device = None;
    let mut context = None;
    // SAFETY: the two out parameters point at locals that outlive the call.
    // The driver type is UNKNOWN because an adapter was named, and no
    // feature levels are asked for, so D3D11 picks what the adapter has.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| classify(&e, "create a D3D11 device"))?;
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(CaptureError::Failed(
            "D3D11 reported success with no device".into(),
        )),
    }
}

/// A texture the size of one monitor that the CPU may read. Staging is the
/// usage that means "no shader will ever touch this, it is here to be
/// copied to and read from".
fn staging_texture(device: &ID3D11Device, size: (u32, u32)) -> Result<ID3D11Texture2D, CaptureError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        // The desktop's own format, so the copy is bytes rather than a
        // conversion, and the same order the framebuffer stores.
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: the description is a local that outlives the call, and the
    // out parameter points at another.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| classify(&e, "create the staging texture"))?;
    texture.ok_or_else(|| CaptureError::Failed("D3D11 reported success with no texture".into()))
}

impl Capture for DxgiCapture {
    fn size(&self) -> (u32, u32) {
        (self.bounds.width() as u32, self.bounds.height() as u32)
    }

    fn screens(&self) -> Vec<Rect> {
        if self.outputs.is_empty() {
            // Between a lost duplication and the rebuild there is no
            // monitor to report; the picture is still its old size.
            let (w, h) = self.size();
            return vec![Rect::new(0, 0, w as i32, h as i32)];
        }
        self.outputs.iter().map(|o| o.rect).collect()
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, CaptureError> {
        if self.rebuild {
            self.build()?;
            self.rebuild = false;
        }
        if self.outputs.is_empty() {
            std::thread::sleep(timeout);
            return Ok(false);
        }
        // A frame apply has not taken yet: nothing to wait for.
        if self.outputs.iter().any(|o| o.held.is_some()) {
            return Ok(true);
        }
        if self.outputs.len() == 1 {
            // One monitor blocks for the whole wait, which is what makes an
            // idle desktop free.
            return self.acquire(0, millis(timeout));
        }
        // Several monitors have to take turns, because a duplication can
        // only be waited on one at a time. Each gets a slice until the
        // deadline; once one has a frame the rest are only polled, so the
        // others' news is not held back by a monitor that is asleep.
        let deadline = Instant::now() + timeout;
        let mut ready = false;
        loop {
            for i in 0..self.outputs.len() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() && !ready {
                    return Ok(false);
                }
                let ms = if ready { 0 } else { millis(left.min(SLICE)) };
                if self.acquire(i, ms)? {
                    ready = true;
                }
            }
            if ready {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }

    fn apply(&mut self, fb: &mut Framebuffer) -> Result<Frame, CaptureError> {
        let mut frame = Frame::default();
        let (width, height) = self.size();
        if fb.width() != width || fb.height() != height {
            fb.resize(width, height);
            frame.resized = true;
            self.refresh = true;
            for out in &mut self.outputs {
                out.first = true;
            }
        }
        if self.refresh {
            // New duplications: the framebuffer holds nothing anyone can
            // trust, so the whole picture is damage and every monitor
            // copies all of itself.
            self.refresh = false;
            frame.damage = Region::from_rect(fb.bounds());
        }

        let DxgiCapture {
            outputs,
            scratch,
            pointer,
            rebuild,
            ..
        } = self;
        scratch.damage.clear();
        let mut outcome = Ok(());
        for out in outputs.iter_mut() {
            if out.held.is_none() {
                continue;
            }
            if let Err(e) = out.apply(fb, scratch, pointer, &mut frame) {
                if matches!(e, CaptureError::Lost(_)) {
                    *rebuild = true;
                }
                outcome = Err(e);
                break;
            }
        }
        if !scratch.damage.is_empty() {
            frame.damage = frame.damage.union(&Region::from_rects(scratch.damage.drain(..)));
        }
        outcome.map(|()| frame)
    }
}
