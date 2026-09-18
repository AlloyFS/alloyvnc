//! Windows: the desktop through DXGI desktop duplication, keys and the
//! pointer through SendInput.
//!
//! Desktop duplication is the compositor's own account of what changed. A
//! frame arrives only when something did, with the rectangles that changed,
//! the blocks that moved (a scroll, a window drag) and the pointer's shape
//! and position. That is why this backend and not GDI polling, which reads
//! the whole screen to find out nothing happened, or Windows.Graphics.Capture,
//! which reports no moves.
//!
//! This is the same mechanism every Windows remote-desktop server uses. It
//! captures the interactive desktop of the user who runs it, and nothing it
//! does is hidden: the server logs every session, binds to loopback unless
//! told otherwise, and demands a password anywhere else.
//!
//! Everything that touches Win32 is gated on Windows. What is left, the
//! keysym table and the pointer-shape conversion, is arithmetic over plain
//! buffers and is compiled and tested on every platform.

pub mod cursor;
pub mod geom;
pub mod keysym;

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod input;

#[cfg(windows)]
pub use capture::DxgiCapture;
#[cfg(windows)]
pub use input::WinInput;

/// Tell Windows this process is shown physical pixels, before anything asks
/// it for a coordinate.
///
/// A process that has not said so is lied to on a scaled display: Windows
/// reports the virtual desktop in scaled units and stretches what it draws,
/// while DXGI hands over the monitor's real pixels either way. The picture
/// would then be one size and the pointer's coordinate space another, and
/// every click would land short. Per-monitor v2 is the strongest of the
/// awareness modes and the one that also keeps the scaling right when
/// monitors differ.
///
/// A failure means the process already has an awareness, usually from a
/// manifest, or that this Windows predates the call. Neither is worth
/// refusing to start over, so the result is dropped.
pub fn init() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };

        // SAFETY: the argument is one of the context values the API
        // defines, and the call takes nothing else.
        if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            tracing::debug!(error = %e, "dpi awareness was already set");
        }
    }
}
